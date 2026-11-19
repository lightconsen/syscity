# Computer Module

Cross-platform unified desktop interface for computer use and desktop automation.

## Design

This layer sits above `CapabilitySet` + `ToolRegistry` and provides a platform-agnostic API for screenshots, UI-tree reading, and desktop actions. The LLM still interacts with individual `Tool`s via `ToolRegistry`; `ComputerAdapter` is consumed by the higher-level `GoalPlanner` and `ComputerUseLoop`.

```
Agent / Planner
      │
ComputerAdapter  ←── Unified interface
      │
ToolRegistry ──→ CapabilitySet ──→ xdotool / SendKeys / AXUIElement
```

- **`ComputerAdapter` trait** — Cross-platform adapter for desktop perception and action
- **`types.rs`** — Core types: `DesktopAction`, `Screenshot`, `UiElement`, `Rect`, `Point`, etc.
- **`system.rs`** — System information gathering
- **`verification.rs`** — `VerificationEngine` with `VerificationCriteria` for task success checking
- **`use_loop.rs`** — `ComputerUseLoop` with `LoopDecision` parsing for iterative desktop interaction
- **`rollback.rs`** — `RollbackManager` with snapshot-based state restoration
- **`headless.rs`** — `HeadlessComputerAdapter` with virtual display support
- **`fs_watch.rs`** — `FileWatcher` for directory and file change monitoring
- **`network.rs`** — `NetworkInspector` for port scanning and firewall inspection
- **`log_aggregator.rs`** — `LogAggregator` for system log collection and alerting
- **`audio.rs`** — `AudioCapture` for audio event detection
- **`screen_recorder.rs`** — `ScreenRecorder` for video capture
- **`screenshot_encoder.rs`** — Screenshot encoding utilities
- **`sensitive_ui.rs`** — Sensitive UI element detection and masking
- **`remote_control.rs`** — `RemoteControlAdapter` for SSH/VNC/RDP remote machines
- **`reflection.rs`** — Self-reflection and state introspection
- **`vision/`** — Visual perception layer (behind the `vision` feature): RapidOCR (`ocr_rapid.rs`) for text and OmniParser (`ui_onnx.rs`) for UI element detection from screenshots, plus `screen_state.rs` capture, `model_download.rs`, and `preprocess.rs`

### Runs (`computer.run`)

A *run* is one goal driven from the gateway: `computer.run` spawns it and returns at once, and
everything after that is `computer.step` events plus `computer.status`. The modules below are that
path, and each one exists because the naive version of it had a specific failure:

- **`driver.rs`** — the decision closure: renders a `LoopState` as a prompt and parses the answer back
  into a `LoopDecision`. Text first: the accessibility outline, not a screenshot. A parse failure is a
  request to re-observe, never a completion.
- **`run_executor.rs`** — the production closure and everything a gateway run needs around it: the
  per-run budget enforced *before* each model call, live state, one event per step, and the handoff.
- **`run_budget.rs`** — per-run cost ceilings (`max_calls_per_run`, `max_images_per_run`) and
  `plan_perception`: an image is attached only when the text cannot carry the question (truncated
  tree, unverified last step, two failures in a row). The eight-image cap on a *request* does not
  bound a thirty-step run.
- **`run_registry.rs`** — the runs themselves, kept on the gateway rather than a connection so one
  client can watch or steer another's run. Finished runs are retained (bounded) so "run, then ask what
  happened" is not a race.
- **`handoff.rs`** — the four-part note posted to the approval queue when a run stops short: done,
  where, tried, next. Advice keyed on *why*, not "please check".
- **`run_store.rs`** — survives a restart. One JSON file per run under
  `<layout root>/computer-runs/<run_id>.json`, written after every step with tmp+rename (a crash
  mid-write leaves the previous checkpoint, not a truncated one). A resume reads it back and continues
  at step N+1 with the same `max_steps` and the same spent budget; a run that reaches an answer
  (`done` / `cancelled`) deletes its file, because a checkpoint is only worth keeping while someone
  might continue it. See [`### Checkpoints and resume`](#checkpoints-and-resume).
- **`tree_render.rs` / `tree_budget.rs`** — the outline's line budget, spent on windows a person can
  actually see: an off-screen tool window with more children than the whole budget (OpenShot's emoji
  panel, 119) used to consume it before the window under test was reached.
- **`image_budget.rs`** — the size and encoding of the one image a step may attach, kept separate from
  `screenshot_encoder`'s *bandwidth* heuristic (which picks the most expensive setting on a local
  machine — the opposite of what a context budget wants).
- **`vision/look.rs` + `vision/resolver.rs`** — who looks, when the tree cannot answer. The ladder is
  local ONNX → API vision model → nobody, configured by `[computer.vision].mode`:
  `auto` (default) / `local` / `api` / `off`. `local` is a privacy promise and never falls through to
  the network; `off` never looks. A scan's output is converted to `UiElement`s with snapshot ids, so
  the visual path and the tree path are isomorphic — and since the adapter never held those ids, the
  run rewrites a target naming one into the point the scan established before dispatch.

```toml
[computer.vision]
mode = "auto"          # auto | local | api | off
provider = "openai"    # the API rung, when it exists
model = "gpt-4o"
model_dir = ""         # empty → ~/.syscity/models/vision
timeout_secs = 10      # one lookup, then degrade — never block the loop
```

### Platform Adapters

| Platform | Module | Backend |
|----------|--------|---------|
| macOS | `platform_macos.rs` | AXUIElement, AppleScript |
| Windows | `platform_windows.rs` | UIAutomation, SendKeys |
| Linux | `platform_linux.rs` | xdotool, AT-SPI |

### Checkpoints and resume

A run takes real actions on somebody's screen, so a gateway restart mid-run must not erase it and must
not silently re-arm it. Both halves of that are the checkpoint file.

**What is written.** After every step the run appends one `CompletedStep` — the action, the
three-state verification, the result line, the CAS digests of the captures *before* and *after* the
action (`screenshot_before_ref` / `screenshot_ref`), which rung of the action ladder finally delivered
it (`rung`, absent when the adapter reported no evidence), and whether the action aimed at a sensitive
control — plus the running `model_calls` / `images_attached`. The order is deliberate: the step goes
into the run's own view, then the file, then the `computer.step` event, so a crash between them loses
at most the report, never the record of an action that already happened. The pre-action frame matters
because it is what the decision was made against: a replay can show what a step *did* without it, never
what it was *looking at*. `computer.runs` therefore exposes a per-step `steps` array
(`{step, action, result, verified, before_ref, after_ref, sensitive}`) whose shape matches the
`turns.replay` timeline, so a run's timeline and a trace's timeline draw the same way.

**When it is deleted.** A run that reaches a conclusion (`done` / `cancelled`) removes its file: there
is nothing to continue, and keeping every finished run's steps would grow the layout forever for no
reader. `max_steps`, `budget` and `need_help` keep it, with the reason recorded — a ceiling means
"more room would help", and a person decides.

**What a failure becomes.** Keeping the file is also what makes the run collectable: the same three
stopping reasons put one row into the pending badcase pool (`eval/pending_badcase.rs`, source
`computer:run`), carrying the goal, the first failed step, the last capture's digest, the rung
distribution, and the handoff note the person was shown. The dedup key is what the run *did* — stop
reason, step number, action, verification — never the run id or a capture digest, so the fifth
repetition of one dead end collapses onto the first row instead of flooding the pool. The conversion
lives in `computer/failure.rs` and the producer in `ws/admin_ws/computer.rs::record_run_badcase`; a
row is only a *candidate* until somebody confirms an end state (`eval.badcase.confirm`), because an
expected end state is a person's judgement, not the model's. See `docs/harness.md` §6.

**What a resume does.** `computer.resume` starts a *new* loop with the checkpoint's history as memory:
it continues at step N+1, keeps the original `max_steps`, and seeds the budget counters
(`RunBudget::restore`) so a restart cannot double the caller's allowance. Completed actions are never
replayed — the history is memory, not a script. The recorded post-action captures are *not* re-attached;
the digests stay in `screenshot_before_ref` / `screenshot_ref` for the replay path instead of spending
the resumed turn's context on frames the decision maker already acted on.

**Sensitive steps.** A checkpoint that recorded a step aimed at a password / payment / destructive
confirmation requires `confirm_sensitive: true` on the resume. The person approving the resume is
approving the goal, not the password field a previous model call happened to aim at.

**Startup.** Nothing is auto-resumed (`lifecycle.rs`, same rule as persisted goals). The gateway logs
the count, writes a line into each run's originating session naming the run and the step to continue
from, and `computer.runs` is the list behind it. Both live in `ws/admin_ws/computer.rs::launch`, which
owns every pre-spawn refusal so `computer.run` and `computer.resume` cannot diverge.

### Reading a turn back

Two questions a person asks after a desktop turn, and two screens (both read-only, both in `web/`):

- **What is happening now** — `ComputerPanel.tsx` in the right pane, fed by `computer.step` events
  while the run is live and by `computer.runs` (the checkpoints above) after it. Filtered by
  `session_id`: a run answers to the conversation that started it.
- **What did that turn actually do** — `TurnReplayPlayer.tsx`, a dialog opened from an assistant
  message's action bar, fed by `turns.replay { turn_id }`. The turn's own `computer`-tool steps and the
  steps of every run it started come back as one list, in the order they happened; each step carries
  its CAS digests, and the player fetches exactly one frame at a time via `attachment.get` — for the
  step being looked at. A turn's trace is written by the gateway, so the dialog needs no desktop
  adapter; `Checkpoint.turn_id` is the join that lets a run's steps appear in its turn's timeline.

Neither screen can start, pause or resume a run. The point of both is that a person can see what
already happened; a replay that could re-run the turn would be a second way to drive a machine.

### Recording and replay

A run answers "do this goal". A workflow answers "do what I just did, with a different input" — the
second time is not a fresh decision, it is the same actions with a value substituted.

**What gets recorded.** `computer.record_start` opens one `Recording` on the gateway's tool state
(`Recording` in `planner/workflow.rs`), and the `computer` tool feeds it from
`tools/computer.rs::record_step_if_recording`. Three questions decide whether a call is a step:

- a recording is running, and the action is replayable (`screenshot` / `read_ui_tree` are how a
  decision was made, not what it did);
- the action landed: `result.success`, verification not `Failed`, effect not `Refused`;
- the action can be named later — `stabilize` resolves each element id into `role + label` *at record
  time*, through the same adapter resolution the step itself used. A snapshot id names nothing after
  the next read; a role and a label survive into a run that will never see this process's tree again.

The handle lives on the gateway, not on the tool or the connection, because the actions arrive from an
agent turn's registry (a different `ComputerTool`) while the recording was started over WS. Sharing
the handle is what makes "record a task by asking the agent to do it" work; one recording at a time
follows from there, since there is one screen.

**What is saved.** `computer.record_stop` calls `Workflow::parameterize` first: every `Type` step's
literal (trimmed, ≤200 chars, single-line) becomes `{{name}}`, named after the target control's label
(`text` when there is none), and both the action *and* the step description are rewritten. The
description is what the replay's run log prints, so leaving the literal in it would show a reader the
recorded value while the field holds the bound one. The parameterised `Workflow` is then written to
`~/.syscity/workflows/<stem>.json` (tmp + rename), and the response names the parameters so a caller
can bind them without opening the file.

**Before a replay moves: the ambiguity preflight.** A recorded workflow aims by *name*, because
`stabilize` rewrites every element id into `role + label` at record time — an id does not survive a
restart. A name is not an identity, and the screen a replay runs on is not the screen it was recorded
on: if it now holds two "New Folder" buttons, the recorded step is a coin flip nobody authorised.

`computer/replay_preflight.rs` resolves every target against the current tree at step 0 and refuses the
whole replay with `REPLAY_AMBIGUOUS` if any of them has more than one match. The message names the
step, the count and each candidate's id, so the way out (re-record aimed at an id, or make the targets
distinguishable) is in the refusal rather than in a step log the person has to reconstruct.

Two deliberate limits:

- **Ambiguity, not existence.** Most workflows open their own window in step 1 (`launch Finder`, then
  click inside it), so demanding that targets exist at replay start would refuse exactly the workflows
  this protects. A target that is merely absent is left to the step that finds it missing.
- **A tree that cannot be read is not a refusal.** The replay runs and each step resolves its own
  target as before; losing the preflight costs the early warning, not the replay.

This is not the same check the ladder already makes. `ComputerSession` refuses an ambiguous target at
the moment it runs (nothing is dispatched, which is right and too late): it surfaces as step N failing
for a reason the person reconstructs after N-1 steps have touched the screen.

**What a replay does.** `computer.play` refuses before anything moves when the workflow is missing,
when a parameter is unbound (blank counts as unbound — it would type nothing), or when a step needs a
person and `confirm_sensitive` was not given. `needs_confirmation` is true for a step that aimed at a
sensitive control and for one that is destructive by shape (`close_window`, `kill_process`,
`restart_process`, shell); the approval that let the recording happen is not a standing permission to
do it again. The replay then runs like a run: a `run_id` back immediately, one `ComputerSession` for
the whole workflow, `controller.check_and_wait()` between steps, each step's line pushed into the
run's `completed` and broadcast as `computer.step`.

A step's failure escalates the same way it does in a run, and the verdict is read the same way:
`Failed` verification means the action ran and the screen did not change, which is a failed step;
`Unverifiable` is a success carrying `None`, because counting "nothing could check it" as a failure
retries a step into doing what it already did. `FailureStrategy` (`skip` default, `abort`, `retry`)
decides whether the replay goes on; a `Refused` step is terminal for that step whatever the strategy,
since the same request meets the same refusal.

**Watching without touching (stage G).** SSH already drives a remote machine; what was missing is a
person being able to *see* it, which is the difference between approving a click and checking one.
`computer.watch` captures on a timer and broadcasts each capture as `computer.frame`;
`computer.unwatch` stops it. `computer/watch.rs` holds the frame contract and `computer/watch_registry.rs`
the live set (on the gateway's tool state, like the recording — one screen, so a watcher started from
one connection must be stoppable from another).

"See, do not drive" is structural: that module has no path that dispatches a desktop action, so it is
not a flag someone can flip. Frames are ordinary `Screenshot`s — the same capture path that already
works locally and over SSH — published as ready-to-display data URLs, with the media type read from the
payload's own first bytes (the capture path may have re-encoded it to JPEG, and a wrong type turns a
good frame into a broken icon). A failed capture is broadcast as `computer.frame_failed` rather than
skipped, because a viewer looking at a frozen last frame has to be able to tell "nothing changed" from
"the capture broke"; three consecutive failures end the watcher, so a host that cannot capture at all
does not log at the frame rate for five minutes. A watcher also ends itself after five minutes — "on"
must not be a state that outlives the person who asked — and every loop `select!`s on shutdown.

**The video half.** The actions are what a replay reproduces; the picture is what a person needs when
the replay then does the wrong thing. `[computer] record_video = true` (off by default) makes
`computer.record_start` also start a screen capture, and `record_stop` records the resulting file on
the workflow as `video` (relative to the artifacts directory, so a moved root still resolves).

The capture lives in `computer/recording.rs` and is three parts, each for a reason:

- `ScreenRecorder` already reads the display, but its buffer is a *window* (`max_buffer_secs`) for
  asking "is the scene stable", not the whole recording. A recording may last minutes, so a
  `StreamSink` (`with_stream_sink`) hands every frame onward as it arrives, before the buffer's own
  trim can drop it.
- `StreamingEncoder` is that frame's destination: one ffmpeg process, fed frame by frame, so what the
  recording holds is a pipe rather than gigabytes (1920×1080 RGBA is ~8 MB a frame, 40 MB a second at
  5 fps). The encoder cannot start until the first frame reports the dimensions.
- The supervisor polls `Recording::session_id()` and closes the file when the recording ends. Polling
  rather than hooking `record_stop` is deliberate: `record_stop` is one of several ways a recording
  ends (the same handle is dropped at shutdown), and a video that stops only when someone rings a bell
  is truncated whenever they forget.

A capture that cannot start — no ffmpeg, no Screen Recording permission, no display — is a `null`
`video` in the reply and a warning in the log, never a failed `record_start`. Losing the picture but
keeping the replayable workflow is strictly better than losing both, and the two are independent
permissions.



```rust
#[async_trait]
pub trait ComputerAdapter: Send + Sync {
    async fn screenshot(&self, region: Option<Rect>) -> Result<Screenshot>;
    async fn read_ui_tree(&self, app: Option<&str>) -> Result<Vec<UiElement>>;
    async fn execute(&self, action: DesktopAction) -> Result<ActionResult>;
    async fn wait_for(&self, condition: WaitCondition, timeout: Duration) -> Result<bool>;
    // Semantic addressing: an element id resolves back to the element
    async fn resolve_target(&self, target: &ClickTarget) -> Result<ActionTarget>;
    async fn perform_ax_action(&self, target: &ActionTarget, action: AxAction) -> Result<ActionResult>;
    // Background delivery; the default refuses, because a platform with no
    // background mechanism must not silently use the foreground
    fn supports_background_input(&self) -> bool;
    async fn perform_background(&self, action: &DesktopAction, target: Option<&ActionTarget>) -> Result<ActionResult>;
    async fn display_geometry(&self) -> Result<DisplayGeometry>;
    // Convenience wrappers over execute(): click_at, type_text, key_press,
    // clipboard_get, clipboard_set, restart_process, set_process_priority
}

pub enum DesktopAction {
    Screenshot { region: Option<Rect> },
    Click { target: ClickTarget, button: MouseButton },
    DoubleClick { target: ClickTarget, button: MouseButton },
    Type { text: String, target: Option<ClickTarget> },
    KeyPress { keys: Vec<String> },
    Scroll { target: ClickTarget, direction: ScrollDirection, amount: i32 },
    Drag { from: ClickTarget, to: ClickTarget },
    // A hover is a *measurement* — does this control react to the pointer? —
    // not a read of an `enabled` flag the platform may not publish. Because the
    // tooltip it may open covers the next frame, a hover is expected to be
    // followed by `PointerMove` somewhere empty.
    Hover { target: ClickTarget },
    PointerMove { to: Point },
    ReadUiTree { app: Option<String> },
    LaunchApp { name: String, args: Vec<String>, wait_for_ready: bool },
    ActivateWindow { title_pattern: String },
    CloseWindow { title_pattern: String },
    ListWindows,
    GetWindowGeometry { title_pattern: String },
    MoveWindow { title_pattern: String, x: i32, y: i32 },
    ResizeWindow { title_pattern: String, width: u32, height: u32 },
    MinimizeWindow { title_pattern: String },
    MaximizeWindow { title_pattern: String },
    Wait { milliseconds: u64 },
    ClipboardGet,
    ClipboardSet { text: String },
    GetSystemStatus,
    ListProcesses { filter: Option<String>, limit: Option<usize> },
    KillProcess { pid: Option<u32>, name: Option<String>, force: bool },
    RestartProcess { pid: Option<u32>, name: Option<String>, force: bool },
    SetProcessPriority { pid: Option<u32>, name: Option<String>, priority: i32 },
}

pub enum LoopDecision {
    Done { message: String },
    NeedHelp { reason: String },
    Action(DesktopAction),
}
```

## Action ladder

An action that names an element is tried at several rungs, cheapest and most
precise first, until one has a visible effect:

| Rung | Click | Type |
|------|-------|------|
| `ax_action` | `AXPress` on the element | `SetValue` on the field |
| `pixel` | click the element's center | click the field, then type |
| `keyboard` | focus, then press the key the role answers to (Space for buttons, check boxes, switches, tabs; Return otherwise) | type into the focus |
| `page` | ask the DOM by role and name through CDP | fill the element by role and name |

Escalation happens only on `SuspectedNoop` — a rung that ran and changed
nothing. Every step reports an `ActionEffect`: `Confirmed`, `Unverifiable`,
`SuspectedNoop`, `Partial` (delivered but only partly applied — do not retry
blindly) and `Refused` (never delivered; the caller must change the request).
`Partial` and `Refused` carry a `recovery` hint.

The page rung only exists when a browser is attached to the session. Both the
browser tool and the computer tool share one `BrowserPool`, so the page the
tool opened is the page the rung acts on, scoped to the conversation that
opened it. Matching is strict: a name that is not on the page is refused, not
approximated.

### Delivery

`SessionConfig.delivery` (the computer tool's `delivery` argument) chooses
where input may land:

- `foreground` (default) — the pointer may move and focus may change.
- `background` — the pixel and keyboard rungs go through
  `ComputerAdapter::perform_background`, which acts on the element without
  raising the app (on macOS, System Events accessibility scripting: an element
  press or a `set value`, never `activate`). A platform or action with no
  background form is **refused** with a reason; the ladder does not fall back
  to the foreground, because that is exactly what was asked not to happen.
  Clicks at a bare screen point are refused in this mode: an accessibility hit
  test at a point lands in whatever window happens to be in front.

### The app boundary (`[computer] allow_apps`)

`[computer] allow_apps` is the list of applications a desktop action may be
aimed at:

```toml
[computer]
allow_apps = ["Safari", "Google Chrome*", "Code"]
```

Empty — the default — is no boundary. Once it is non-empty, every action that
mutates the screen is checked against it **inside the session, before anything
is captured or dispatched**, and an app that is not on the list is refused
(`ActionEffect::Refused`, not delivered). Matching is case-insensitive and glob
(`*` at either end), because the platform's spelling is not ours: macOS reports
`Google Chrome`, a rule may say `Google Chrome*`.

An action that names no app is judged by the app that would **receive** it.
A coordinate click, a bare key press and typing into whatever holds focus reach
whatever is in front — so before dispatch the session asks the platform who
that is (`ComputerAdapter::application_at` for a point,
`ComputerAdapter::active_app` for the focus) and allows the action when that
app is on the list. When the platform cannot say — no accessibility grant, no
window server, an adapter that has not implemented the query — the answer is
still no, and the refusal says *that* is why instead of pretending the action
named nothing. A process action that names a pid rather than a name is in the
same position: a pid is not an app.

The second half is checked per rung. An action that names an allowed app may
still fall back to the pixel or keyboard rung after the accessibility rung had
no effect, and those two deliver to a point or a focus — the exact moment a
covering window or a stolen focus decides who receives the event. Before either
rung dispatches, the app that would receive it is compared with the app the
action named; a mismatch is refused with the receiving app named. The
accessibility and page rungs are not checked this way: they act on an element,
so the foreground has nothing to do with where they land.

Observation is not gated: screenshot, UI-tree reads, window listing, waits,
clipboard reads and system status all work whatever the list says, because a
model that cannot look cannot find out where it is allowed to go.

The same list feeds `primary_invocation_arg`, so `[permissions].deny` can name
an app (`computer:Safari`, `*_desktop_control:1Password`) and refuse the call
before the tool runs at all. The two are complementary: a rule reaches calls
that name the app, and the session boundary is the half that holds for the
calls that do not.

This is **not isolation**. The actions still run on the real desktop with the
real user's permissions; the list narrows what the tool will do, not what the
process could do. Isolation (a container or VM) is a deliberate non-goal — see
the plan's §6.6.4 — because it turns "visible and takeover-able" into background
batch work.

Changing the list with `config.set` (`path: "computer.allow_apps"`, an array of
names) or by editing `config.toml` reaches **sessions that are already
running**: the handle is shared, not copied, and both paths `replace()` it.

### Approvals

Every platform tool an adapter uses to move the machine — `applescript`,
`*_desktop_control`, `*_powershell`, the clipboard writers — advertises
`requires_approval`. Observation tools (`*_accessibility`, `*_screenshot`) do
not: a model that cannot look cannot find out what to ask about.

The adapter does not ask. It is reached from the `computer` tool or from
`computer.run`, and the caller already answered a gate — asking again for each
of the seventeen inner calls a step makes would be several prompts a second and,
in a run started over WS, no prompt at all. So a session carries the approval
that produced it:

- `SessionConfig.approved_by` names the tool that was approved
  (`computer`, `computer.run`, `computer.play`), and `conversation_id` says
  which conversation's turn it came from.
- `ComputerSession::dispatch` puts that pair in a tokio task-local
  (`DesktopGrant`) around the whole dispatch — foreground *and* background,
  since `perform_background` reaches `applescript` too.
- The registry's gate, before refusing a `requires_approval` call out of a
  context that cannot ask, checks for a grant in scope. If it covers this
  conversation and the tool is on `DESKTOP_INNER_TOOLS`, the call is allowed
  and logged as `kind: "inherited_approval"` with both names, so a run that
  clicked a hundred times is traceable to the one decision that allowed it.

Task-local rather than a field on the adapter: the adapter instance serves every
conversation, and a grant parked on it would be read by whichever task happened
to run next. A tool *not* on `DESKTOP_INNER_TOOLS` never inherits — a `computer`
approval is not a way for a model to reach `shell`. A grant whose conversation
differs from the calling context's is likewise ignored.

`DESKTOP_INNER_TOOLS` is the contract between the two halves, so a sweep in
`agent_spawn` checks it both ways: every name the adapters really call must be
listed (or a step is refused with no way to say yes), and every listed name must
answer to a compiled tool (or a rename has quietly un-gated one).

## Regression baseline

`tests/gui_baseline.rs` drives a real headless Chrome through the page rung —
click by name, fill by name, toggle a check box, follow a link, and refuse a
name that is not on the page — and reports a pass rate. It is opt-in:

```text
SYSCITY_GUI_BASELINE=1 cargo test --test gui_baseline -- --nocapture
```

Ordinary `cargo test` compiles it and skips it, so CI without Chrome is not a
regression.

## Data Flow

```
User Request (desktop task)
    │
    ▼
is_desktop_task() heuristic
    │
    ▼
ComputerUseLoop::run()
    │
    ├──▶ screenshot() ──▶ LLM analysis
    ├──▶ read_ui_tree() ──▶ LLM analysis
    ├──▶ execute(action) ──▶ Platform adapter
    │       │
    │       ├──▶ macOS → AXUIElement
    │       ├──▶ Windows → UIAutomation
    │       └──▶ Linux → xdotool
    │
    └──▶ verification engine ──▶ success check
```

## Implemented Features

- Cross-platform `ComputerAdapter` trait (macOS, Windows, Linux)
- Screenshot capture with optional region selection
- Accessibility UI tree reading
- Desktop actions: click/double-click/drag, type, key press, scroll, wait, clipboard
- Action ladder with per-step evidence (`ax_action → pixel → keyboard → page`), role-aware keyboard activation, and a page rung over CDP
- Background delivery with explicit refusals (macOS element press / value set without activating the app)
- Window management: list/activate/close/move/resize/minimize/maximize windows, app launching
- Process control: list/kill/restart processes, set priority, system status
- File and directory watching
- Network port listing and inspection
- Computer use loop with iterative LLM-driven desktop interaction
- Loop decision parsing (`DONE:`, `HELP:`, `ACTION:` prefixes)
- Verification engine with configurable criteria
- Rollback manager with snapshot-based restoration
- Headless mode with virtual display support
- Remote control via SSH/VNC/RDP
- System log aggregation with alert rules
- Audio capture and event detection
- Screen recording with video frame capture
- Sensitive UI element detection and masking
- Computer vision integration (feature-gated behind `vision`):
  - RapidOCR for on-demand text extraction (`screen_ocr` tool)
  - OmniParser ONNX UI element detection (`screen_ui_detect` tool) returning role, bounds, and confidence for buttons, text fields, checkboxes, icons, and links
  - Automatic OmniParser fallback in the `screen_state` tool when the accessibility tree is empty (games, image-based UIs, remote desktops, webviews); opt out with `ui_fallback=false`. The fallback never runs in `ScreenState::capture_light`, so the cheap verification loop is unaffected. Both detectors load lazily on first use via shared handles.

