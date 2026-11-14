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

## Key Types

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

