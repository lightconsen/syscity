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
| `android_dialog` | describing a dialog: reads its text and buttons, and presses one only if exactly one button matches |
| `android_system_action` | tapping through alarm / timer / calendar / share / settings UI, via Android's own intents |
| `android_ocr` | the screenshot reader, for screens whose accessibility tree describes nothing |

Two behaviours apply across the set. Every `android_input` (and `android_dialog`
press) re-reads the screen afterwards and reports `effect: changed / unchanged /
unknown`, so "did that tap land" no longer costs an `android_observe` turn. And
every tree read is graded `usable / partial / unusable`; below usable the tool
falls back to OCR and returns the text as tappable lines rather than an empty
list.

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
