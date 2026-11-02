# OS Control Architecture

This document describes how Syscity agents perceive and operate the host operating system — covering desktop GUI environments, headless servers, mobile devices, and future physical-world extensions.

## Design Philosophy

The agent should interact with the OS through the same layered abstractions used elsewhere in Syscity:

- **PlatformToolSet** groups platform-specific tools by environment (Linux server, macOS desktop, Windows desktop, Android, iOS).
- **ToolRegistry** manages individual tool lifecycle, permissions, circuit breakers, and approvals.
- **ComputerAdapter** provides a unified cross-platform API for desktop/server perception and action.
- **GoalPlanner** decomposes high-level objectives into task DAGs executed against the adapter.

The LLM always invokes individual `Tool`s; `PlatformToolSet` and `ComputerAdapter` are organizational and abstraction layers, not execution units.

## Core Architecture

```
Agent / GoalPlanner
        │
        ▼
ComputerAdapter ──▶ unified API: screenshot, read_ui_tree, execute, wait_for
        │
        ▼
ToolRegistry ──▶ PlatformToolSet ──▶ platform tools
        │                              │
        │                              ├──▶ macOS: AXUIElement, AppleScript
        │                              ├──▶ Windows: UI Automation, PowerShell
        │                              ├──▶ Linux: at-spi2, xdotool, systemd
        │                              └──▶ Mobile: ADB, libimobiledevice
        │
        └──▶ SandboxInterceptor, ApprovalQueue, AuditLogger
```

## OS Control Scope

Permission levels restrict what OS actions an agent may perform. See `src/computer/platform/mod.rs::OsControlScope`.

| Scope | Meaning | Example Operations |
|-------|---------|-------------------|
| `ReadOnly` | Observe only | `ps aux`, `journalctl`, `read_ui_tree` |
| `UserSpace` | User-level changes | launch apps, edit user files, manage user services |
| `System` | System-level changes | `systemctl restart`, `apt install`, firewall rules |
| `Root` | Full control | kernel params, user management, destructive operations |

Higher-scope actions require explicit configuration and may trigger the approval queue.

## Desktop Control (GUI Environments)

For systems with a display server, the agent uses a **hybrid perception** approach: structured accessibility UI trees as the primary signal, screenshots as visual validation.

### Perception Loop

```
1. read_ui_tree() ──▶ structured accessibility tree
2. screenshot() ──▶ visual context (optional)
3. LLM analyzes UI tree + screenshot
4. execute(DesktopAction) via Accessibility API
5. wait_for() state change
6. re-read UI tree to verify
```

### Platform Implementations

| Capability | Linux X11 | Linux Wayland | macOS | Windows |
|------------|-----------|---------------|-------|---------|
| UI tree | `at-spi2` | `xdg-desktop-portal` (limited) | `AXUIElement` | `UI Automation` |
| Screenshot | `scrot`/`grim` | `grim`/`xdg-desktop-portal` | `screencapture` | `BitBlt` |
| Click/type | `xdotool` | `ydotool`/`portal` | `AXUIElement` | `UIA InvokePattern` |
| Window mgmt | `wmctrl`/`swaymsg` | compositor-specific | AppleScript/AX | `FindWindow` |

### Key Types

```rust
#[async_trait]
pub trait ComputerAdapter: Send + Sync {
    async fn screenshot(&self, region: Option<Rect>) -> Result<Screenshot>;
    async fn read_ui_tree(&self, app: Option<&str>) -> Result<Vec<UiElement>>;
    async fn execute(&self, action: DesktopAction) -> Result<ActionResult>;
    async fn wait_for(&self, condition: WaitCondition, timeout: Duration) -> Result<bool>;
}

pub enum DesktopAction {
    Screenshot { region: Option<Rect> },
    Click { target: ClickTarget, button: MouseButton },
    Type { text: String },
    KeyPress { keys: Vec<String> },
    Scroll { direction: ScrollDirection, amount: i32 },
    Drag { from: Point, to: Point },
    LaunchApp { name: String, args: Vec<String>, wait_for_ready: bool },
    ActivateWindow { title_pattern: String },
}

pub struct UiElement {
    pub id: String,
    pub role: String,
    pub label: Option<String>,
    pub value: Option<String>,
    pub bounds: Rect,
    pub enabled: bool,
    pub focused: bool,
    pub children: Vec<UiElement>,
}
```

### Implemented Features

- Cross-platform `ComputerAdapter` trait with macOS, Windows, Linux adapters
- Screenshot capture with region selection and encoding optimization
- Accessibility UI tree reading
- Desktop actions: click, type, key press, scroll, drag, launch app, activate window
- Headless mode with virtual display support (`HeadlessComputerAdapter`, Xvfb on Linux)
- Coordinate abstraction with DPI scaling (`Point`, `Rect`, `DpiScale`)
- Verification engine with UI-tree, screenshot-diff, process, and window criteria
- Rollback manager with file backups and system-level snapshots (APFS/Btrfs/System Restore)
- Sensitive UI element detection and masking
- Screen recorder and audio capture
- Vision-based UI detection (`screen_ui_detect` via OmniParser), including an automatic fallback when the accessibility tree comes back empty
- Remote control adapter (SSH-based) for Linux/macOS/Windows

## Server Control (Headless Environments)

For systems without a GUI, the agent acts as a senior sysadmin: collecting structured system state and executing safe, auditable commands.

### Capability Sets

| Set | Environment | Tools |
|-----|-------------|-------|
| `LinuxSet` | Linux server, no GUI | `system_inspect`, `service_manager`, `log_analyzer`, `network_diag`, `package_manager`, `user_manager`, `firewall_manager`, `cron_manager` |

### Server Operator Loop

```
system_inspect() ──▶ SystemSnapshot
    │
    ▼
LLM diagnoses state
    │
    ▼
decide action ──▶ service_manager / package_manager / shell
    │
    ▼
re-inspect to verify
```

### Implemented Features

- `SystemInspector` collecting processes, services, network, storage, users, logs, packages, kernel params, security status
- Structured JSON snapshots
- Service management, log analysis, network diagnostics, package management
- User, firewall, and cron management tools
- File-system change watching and log aggregation

## Mobile Device Control

| Platform | Connection | Capabilities |
|----------|------------|--------------|
| Android | ADB | screenshot, tap/swipe, input, app install/launch/force-stop, UI tree dump |
| iOS | libimobiledevice | device list, screenshot, app management |

These are implemented as additional `PlatformToolSet`s registered alongside desktop/server sets.

The Android set carries five higher-level tools on top of the raw ones, each of
which replaces a turn the model would otherwise spend driving them by hand:

| Tool | What it removes |
|---|---|
| `android_collect` | read-scroll-read over a long list or chat: one call returns the text in screen order, and says why it stopped |
| `android_find` | scrolling until something appears: returns the coordinates once it does, or says how far it looked |
| `android_query` | reading the whole tree to answer "is there a 发送 button" or "how many rows": matches by text / resource id / class and returns one line per hit, with the total |
| `android_wait` | "observe, then observe again a second later": waits for an element to appear or go, or for the screen to settle, and on timeout says what is actually there |
| `android_dialog` | describing a dialog: reads its text and buttons, and presses one only if exactly one button matches |
| `android_system_action` | tapping through alarm / timer / calendar / share / settings UI, via Android's own intents |
| `android_ocr` | the screenshot reader, for screens whose accessibility tree describes nothing |

Two behaviours apply across the set. Every `android_input` (and `android_dialog`
press) re-reads the screen afterwards and reports `effect: changed / unchanged /
unknown`, so "did that tap land" no longer costs an `android_observe` turn. And
every tree read is graded `usable / partial / unusable`; below usable the tool
falls back to OCR and returns the text as tappable lines rather than an empty
list.

### What a tap is about to do

A wrong screen is recoverable. The right screen with the wrong button under the
finger is not: a message reaches someone it cannot be unsent to, money leaves an
account, a delete is gone. Before `android_input` dispatches a tap, and before
`android_dialog` presses a button, the label of the element receiving it is
classified (`computer::platform::mobile::risk`) into `send`, `submit`, `pay`,
`delete` or `order`. It is a word list, and it is tuned to be wrong toward asking:
a hit reports the word that caused it, so a refusal can be argued with.

Two shapes of label are deliberately not classified, because they are far more
common on a screen than the button itself:

- **a status** — "待付款", "已付款", "付款成功", "订单明细", "付款方式". Asking on
  those would mean asking permission to read an order page.
- **a place** — "评论列表", "设置". `评论` and `评论(23)` are the reply button and
  are classified; the count is decoration and does not change what a press does.

The one thing the word list must get right in the other direction is the
compound: "提交订单" is an *order*, not a submit — a submit is the one kind a
deployment may choose to wave through (`[computer.mobile] ask_kinds`), and a
"提交订单" button classified as a submit would be waved through with it.

Then the gate (`mobile::gate`) decides whether the tap may run:

| Situation | What happens |
|---|---|
| a human is reachable, and `ask_kinds` lists this kind | the question is put to them; only an explicit yes proceeds, and a question that times out is a refusal |
| nobody is reachable (cron, goals, a delegated child) | it runs, and the result carries what it was judged to be |
| `ask_before_irreversible = false` | it runs, with the same `risk` on the result |

The unattended case is deliberate. Refusing would strand every scheduled job —
the message due at 9am does not go out because nobody was at the desk. What
bounds an agent with nobody watching is pacing, not refusal.

```toml
[computer.mobile]
# Ask when someone can answer. Off: the taps run, still reporting `risk`.
ask_before_irreversible = true
# Only these kinds are asked about. `submit` is intentionally absent.
ask_kinds = ["send", "pay", "delete", "order"]
# Space out repeated outward actions like a person would. See below.
pace_outward = true
```

Every `android_input`/`android_dialog` result carries `risk` (a sentence naming
what the tap looked like) and `risk_approved` (`true` when a human said yes, and
therefore also absent when there was nobody to ask).

### The pace of actions that reach outward

A send is one tap; a loop of sends is what gets an account flagged. After an
action judged `send`, `submit`, `pay`, `delete` or `order`, the next outward
action in the same app waits (`mobile::pacing`), the way someone working
through a list would:

- the first in an app does not wait;
- the next comes 8–20 s after the previous one, the range growing 0.5/1 s per
  action done in the last 30 minutes, capped at 30 s;
- the gap is measured from the previous action *finishing*, so twenty seconds
  spent reading the screen owes nothing — only the shortfall is waited out;
- each app has its own history; a task that resumes after lunch starts fresh.

The numbers and the shape are Mika's `OutwardPacing`, adopted rather than
invented, so the two products behave the same when a model is moved between
them. The process-wide history is shared by all agents on a host: one phone is
one place, and two slow agents would otherwise be one fast one.

### Where the tree comes from

Two sources produce the same `UiElement` list, so everything above works
identically whichever one answered:

| Source | When | Cost |
|---|---|---|
| **Accessibility service** (in-process, `SyscityAccessibilityService`) | the host *is* the phone and the user has enabled it in Settings › Accessibility | one bridge call |
| **`uiautomator dump`** (ADB) | every other case: a desktop driving a remote phone, or the phone with the service off | a subprocess plus an `/sdcard` round trip |

The result says which one it used (`source`), because a read that disagrees with
what you expected should be traceable to where it came from. The same
in-process path reads a virtual display: pass `display: <id>` to any of the
screen tools and the service selects that display's windows instead.

### A second screen (`android_display`)

A task can run on its own display, so the user keeps the phone. The agent drives
it with `android_display`: `start` creates it and returns an id, `launch` opens
an app on it, `frame` returns a PNG, `tap`/`swipe`/`text`/`key` act on it, `stop`
tears it down. Three more answer questions the model would otherwise have to
guess at:

- **`limits`** — how many displays may be open at once and why not more (API 34
  for per-display focus, a low-memory phone, a display that already failed to
  create).
- **`paste`** — the clipboard route for non-ASCII text. `text` goes through the
  device's key character map and drops or mangles CJK; a paste carries whatever
  the app can render, and the helper puts the user's own clipboard back
  afterwards — but only if it could read it first. `CLIPBOARD_UNREADABLE` means
  the user's clipboard was left alone because the platform would not hand it
  over.
- **`status`/`restart`** — whether the helper is answering, and replacing a
  stale one that outlived the app that started it.

Two fingers (`android_input action: "gesture"`) is only dispatched on a second
display. A pinch has no `input` equivalent, so it is built as `MotionEvent`s and
injected through `InputManagerGlobal` — and the dispatcher validates the pointer
count against the action: `ACTION_DOWN` carries one pointer, each added finger
arrives as `ACTION_POINTER_DOWN`, and they leave in reverse. A `MotionEvent` that
names two pointers on `ACTION_DOWN` is refused with `invalid pointer count 2`,
which is what the first attempt at a pinch hit. The events are also paced to the
wall clock — injected instantly, they arrive with timestamps in the future and an
app's scroll detector does not see a drag at all.

A second display is meant to keep the user out of the way, but an action can
still reach them — a mis-targeted tap, a link that opens the browser, an app
that ignores the display argument and starts itself on the default one. Every
input action therefore carries a **spill**: `android_input` opens a watch on
display 0 before acting, and afterwards reports what rose to the top of the
user's screen *during* that window. The window is the whole mechanism, and it
is why the answer cannot be a single look before or after:

- what was already on the user's screen when the action began is not blamed —
  they were there first, and moving that app to the agent's display would take
  the user's own work away;
- an app that appears mid-action is a spill, named with the reason it was moved
  back;
- an in-app page change on the same task counts too, so a link that navigates
  is caught even though no new task was created.

The helper records arrivals as they happen (`MainScreenWatch`/`MainScreenLog`);
the host only reads them at the end, because a polling loop on the host misses
anything shorter than its own interval. The log is pruned to a fixed window, so
a phone running for a long time does not accumulate task history. One thing the
parse must get right: `dumpsys activity activities` repeats every task under
`ActivityTaskSupervisor state:`, and reading that block put the user's task on
the agent's display — a real spill then looked like nothing had happened.

Creating a display needs `CAPTURE_VIDEO_OUTPUT`, which is `signature|privileged`
— an app can never hold it. So the work happens in a helper that runs as
**shell** (uid 2000), started over the loopback-ADB pairing (§4.5):

```bash
adb shell CLASSPATH=<app.apk> app_process /system/bin \
    net.syscity.desktop.shell.HelperMain <package> <appUid>
```

`android_display start` does that for you: it asks the helper, starts it if it is
not there (`pm path` for the APK, then a detached `app_process` with a nonce in
its environment), and waits for the Binder it hands back through the app's
exported provider. The helper has no socket: SELinux forbids an `untrusted_app`
from opening a `shell` unix socket, and the Binder handoff runs in the direction
the policy allows.

Three things about that call are not obvious, and all three fail the same way —
`SecurityException`, or a display that exists but has nothing on it:

- **`DisplayManagerGlobal` no longer has the shape it used to.** On API 34+ its
  only `createVirtualDisplay` is `(Context, MediaProjection, VirtualDisplayConfig,
  Callback, Executor)`; the `(String, int, int, int, Surface, int, …)` overload
  is gone from that class. The helper uses that one, building the config by
  reflection, and keeps the older shapes as fallbacks.
- **The display service attributes the call to a package** and rejects it when
  that package's uid is not the caller's. A Context without an Activity reports
  `android` (uid 1000), so the call is refused with `packageName must match the
  calling uid`; naming `com.android.shell` — the package that actually owns uid
  2000 — is both true and what the check wants.
- **A Context needs a Looper.** `ActivityThread.systemMain()`, the way to a
  Context with no Activity, creates a Handler, and `app_process` starts with no
  main Looper.

The helper owns the `ImageReader` behind the display, which is why frames leave
as PNG — a `Surface` cannot cross the Binder handoff.

### Checking it on one device

The parts that only a device can answer — which hidden `createVirtualDisplay`
overload this build accepts, whether an app starts on a non-default display,
whether input follows the display id — are the tool actions themselves:

```bash
# health of the helper, and what it says it can do
android_display action=status
# how many displays this phone will run, and why not more
android_display action=limits
# the whole sequence, by hand:
android_display action=start  → id
android_display action=launch display=<id> package=com.android.settings
android_display action=frame  display=<id>
android_input  action=tap    display=<id> x=540 y=1200 verify=true
android_display action=stop   display=<id>
```

`start` failing names the reason: `NO_HELPER` (nothing is attached; `restart`
replaces a stale one) or `CREATE_FAILED` (the helper refused to make the
display; the message says what it returned, and it is version-specific).
`launch` failing is `LAUNCH_FAILED`, and `frame` with no picture is `NO_FRAME`.

`scripts/android-display-check.sh` drives the same sequence over adb and turns
the output into a verdict:

```bash
./scripts/android-display-check.sh                 # build, install, check
./scripts/android-display-check.sh --skip-build    # reuse the last APK
./scripts/android-display-check.sh --device emulator-5554
```

Known limits, in the order they will bite:

- **Android 13** has no per-display focus, so the user's screen can lose focus
  briefly while the agent types; 14+ gets `FLAG_OWN_FOCUS`.
- **Frames cost a PNG each.** Right for one observation per turn, wrong for
  video.
- **Reading the second display needs API 30+.** `android_ui_tree`,
  `android_observe`, `android_input` and the rest take a `display` argument;
  on that display the tree comes from the accessibility service scoped to it
  (windows are selected by display id, which arrived in API 30) and the
  picture from the helper's frame. Below API 30, or with the service off, the
  read fails with `DISPLAY_TREE_UNAVAILABLE` and `frame` + OCR is what is
  left.

## Capability Registry

`PlatformCapabilityRegistry` holds all registered sets, checks platform constraints at runtime, and exports available tools into `ToolRegistry`.

```rust
pub struct PlatformCapabilityRegistry {
    sets: Vec<Box<dyn PlatformToolSet>>,
    disabled: HashSet<String>,
    availability_cache: RwLock<HashMap<String, bool>>,
}

impl PlatformCapabilityRegistry {
    pub fn register(&mut self, set: Box<dyn PlatformToolSet>);
    pub fn available_sets(&self) -> Vec<&dyn PlatformToolSet>;
    pub fn export_to_tool_registry(&self, registry: &mut ToolRegistry, strategy: ToolConflictStrategy);
    pub fn export_with_scope(&self, registry: &mut ToolRegistry, max_scope: OsControlScope, strategy: ToolConflictStrategy);
}
```

### Capability Profiles

Pre-defined profiles make it easy to constrain what the agent can do:

| Profile | Behavior |
|---------|----------|
| `Minimal` | Generic tools only, no OS control |
| `Observer` | Read-only OS tools |
| `Server` | Non-GUI server sets |
| `Desktop` | GUI sets only |
| `Full` | All available sets (default) |
| `Custom` | Explicit list of set IDs |

## Safety and Verification

- **SandboxInterceptor** enforces path allowlists, command blacklists, domain/IP allowlists, and resource quotas before tool execution.
- **Kernel write fences** (`src/tools/process_runner.rs`, `src/tools/win_appcontainer.rs`) confine `workspace_only` command tools (`shell`, `process`, `code_exec`) at the OS level: macOS Seatbelt (`sandbox-exec` profile denying writes outside the workspace), Linux Landlock (in-place `pre_exec` restriction of write rights), and Windows AppContainer + Job object (Low-integrity token, DACL + mandatory label grant on the workspace, `KILL_ON_JOB_CLOSE`). Unlike the parent-side path check, the kernel denies the write no matter what the spawned command does; requests fail closed when the platform sandbox is unavailable.
- **Process-group kill** on timeout: Unix spawns run as process-group leaders and timeouts SIGKILL the whole group, so detached grandchildren cannot survive (or hold output pipes open) after the parent is reaped.
- **Approval queue** gates high-scope or high-risk actions behind human confirmation.
- **VerificationEngine** automatically checks the result of an action and retries on failure.
- **RollbackManager** snapshots files/system state before destructive operations and rolls back on failure.
- **ContentFilter** scans screenshots and command outputs for secrets and PII.
- **Audit logging** records all OS control actions.

## Roadmap

### Implemented

- Accessibility API integration (macOS, Windows, Linux X11/Wayland)
- Unified `ComputerAdapter` with platform adapters
- Hybrid desktop perception (UI tree + screenshot)
- Browser automation via CDP (`chromiumoxide`)
- Verification, rollback, and sandboxing
- Goal planner with DAG execution and persistent task queues
- Mobile bridge (Android ADB, iOS libimobiledevice)
- Headless mode with virtual display

### Remaining / Future

| Area | Items |
|------|-------|
| Embedded / IoT | GPIO control (Raspberry Pi/Jetson), Home Assistant integration, serial/USB device communication |
| Robotics | ROS2 bridge, robotic arm control via SDK |
| Remote desktop | Native VNC/RDP frame-buffer protocols |
| Screenshot optimization | Dynamic resolution/quality based on network conditions |

## Module Relationships

```
src/computer/platform/  # platform tool sets and PlatformCapabilityRegistry
src/computer/           # unified adapter, verification, rollback, headless, remote
src/planner/            # goal decomposition, DAG execution, persistent queues
src/tools/              # individual tools and ToolRegistry
src/security/           # sandbox, audit, content filtering
```
