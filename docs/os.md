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

The Android set carries a set of higher-level tools on top of the raw ones,
each of which replaces turns the model would otherwise spend driving them by
hand:

| Tool | What it removes |
|---|---|
| `android_collect` | read-scroll-read over a long list or chat: one call returns the text in screen order, and says why it stopped |
| `android_find` | scrolling until something appears: returns the coordinates once it does, or says how far it looked |
| `android_query` | reading the whole tree to answer "is there a 发送 button" or "how many rows": matches by text / resource id / class and returns one line per hit, with the total |
| `android_wait` | "observe, then observe again a second later": waits for an element to appear or go, for the screen to settle, or for the phone to report any change at all (`condition: event`), and on timeout says what is actually there |
| `android_extract` | reading a screen as text: `text` returns readable lines in order, `list` one entry per row of a list, `form` one entry per input field with its label and inferred kind (a password's contents are never read) |
| `android_run` | one turn per tap: runs up to 30 known steps in one call, finding each target again on the live screen before acting, and stops at the first step whose aftermath says the rest are unsafe — an app switch, an app landing on the user's screen, or a screen the tree cannot describe |
| `android_event` | polling for "did anything happen": reads the accessibility service's log of window changes, content changes and notifications as facts — kind, app, time, and the notification's text — with a cursor so the next call returns only what is new |
| `android_dialog` | describing a dialog: reads its text and buttons, and presses one only if exactly one button matches |
| `android_system_action` | tapping through alarm / timer / calendar / share / settings UI, via Android's own intents |
| `android_ocr` | the screenshot reader, for screens whose accessibility tree describes nothing |
| `android_web` | a page drawn in JavaScript: opens it in the phone's own WebView and reads and presses its DOM, which no accessibility node describes |

`android_run` delegates every step to `android_input` / `android_app_manager`,
so the risk check, approval gate, repetition ladder and app lease still see each
step individually — a compound action does not get to skip the checks a single
call goes through. It also reports which read path it worked through: the
in-process accessibility service answers in milliseconds and allows 30 steps,
while the `uiautomator` fallback costs a subprocess and an SD-card round trip
per read, so it caps a run at 10 steps and says so.

The event stream has two halves and the tool always says which one answered.
On the phone itself, with the accessibility service on, the service **pushes**
each event into Rust through a JNI callback, so `android_wait condition=event`
is a `select!` between the next event and the deadline — nothing polls, and no
Binder thread is held. When the service is off, or the host is a desktop driving
the phone over ADB, only the service's bounded ring (512 events) is readable, so
the wait polls that ring instead and reports `source: buffer`; with no service
at all the answer is `source: unavailable` rather than a silent timeout.

An event is a fact about *when*, never a picture of the screen: it does not
carry the tree, so a waiting turn still reads the screen at the moment it
returns. Notification events arrive once, when a notification is posted;
listing or dismissing notifications is a different grant
(`NotificationListenerService`) and is not part of this.

The same stream shortens the other two tools. `android_run` no longer re-reads
the tree after every acting step just to learn whether the app changed — the
window event says so, and its result reports `app_change_source: events` when
that path was used (`tree` when no stream exists, in which case the read is
still taken). A run only trusts the stream when the service is on and the
running binding actually delivers window events; otherwise silence would mean
"nothing was sent", not "nothing happened". `android_input`'s effect check
likewise starts from a content event and waits for 400 ms of quiet, then reads
the tree once to say what the screen became — instead of reading it twice to
prove it stopped changing. Both fall back to the polling behaviour when the
stream is absent, and both say which path answered.

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

### An approval is a description, not a mood

A question is open for as long as the person takes to read it, and a phone screen
moves under it: a message arrives and the row under the finger becomes a
different row. So a yes is recorded as **what it was about**, not as permission
to do the next thing:

```rust
struct Grant { package: Option<String>, risk: Risk, label: Option<String>, subject: String }
```

Right before dispatch, when — and only when — somebody actually said yes, the
screen is read again and the tap is classified again. `gate::verify` refuses if
the app is different, if the kind of action is different, or if the element is
not the one described, and the refusal says which. An approval for "发送 in
com.example.chat" does not become an approval for the same label in a mail app,
and it does not become one for a different button that happens to be there now.

The re-read is deliberately conditional. Nobody asked, nobody's answer needs
re-checking, and paying for an extra tree read on every unattended tap would be
cost with no question attached.

### The ladder in front of a repeated action

An agent on a phone fails in three shapes, and they are not the same shape:

- **the same action, over and over** — a tap that worked once is retried because
  the model misread the result;
- **no effect** — the actions differ, the screen does not move: a disabled
  button, a field that will not focus, a list scrolled to the end;
- **going in circles** — two or three screens cycling: open the sheet, close the
  sheet, open the sheet.

`mobile::guard` counts all three per conversation per screen, and answers with a
ladder: a note the model can still act on, then a stronger note, then a refusal
that says to read the screen and do something else. The numbers are Mika's
`GuardRules` — repeats at 3/6/12, a no-effect streak at 3/6/9, a cycle of period
2 or 3 at 6/12 — over a 20-minute window, and only `Unchanged` counts as "no
effect": a screen that could not be re-read is not evidence that it did not move,
and counting it would punish exactly the apps whose trees are hardest to read.

Two ordering decisions carry the design:

- **The ladder runs before the gate.** Asking somebody to approve the twelfth
  identical send is asking them to endorse a loop the agent should have caught
  itself. When the ladder refuses an action that was already approved, the result
  says so — the yes was not used, and nothing was sent.
- **A blocked action is not recorded.** The ladder counts what an action did;
  an action that never ran is not evidence it should be tried again. That is also
  why the readiness check below runs first.

The bucket is keyed by conversation *and* screen, so two conversations driving
the same phone do not stop each other for doing different things, while one
conversation cannot reset its own count by reconnecting. A bucket that goes
quiet is forgotten, so coming back to an app an hour later starts at the bottom
rung.

### Is the phone still where you left it?

Three things can be wrong between one step and the next, and none of them is
"the last action failed":

| State | Code | What the model is told |
|---|---|---|
| the display helper is gone | `HELPER_DOWN` | start it (`android_display action 'start'`) and re-read; the display's contents did not survive |
| the person's screen is locked or asleep | `PHONE_LOCKED` | wait for them to unlock; a second display is not covered by the lock screen |
| the display no longer holds the app it held | `DISPLAY_MOVED` | read it again and recompute the coordinates |

The first fact (a helper that answers at all) is visible from the host; the other
two are not, so the helper reports them (`readiness()` → `{locked, main,
displays[]}`) and the *host* decides. Refusing is policy, and keeping the
judgement on one side is what lets the rule be tested without a phone.

The check runs before the write, not after: a tap that lands on the lock screen
is reported by the effect check as "the screen changed", which reads as success.
The user's own screen is only checked for the lock — it is *supposed* to change
under the agent, and a drift there is what the effect check is for. A display
whose app is gone is only a fault for a write that assumed it was still there;
`launch` is the ordinary way back in and is exempt.

A host driving a phone over ADB cannot see either fact, and is not checked
against a guess: `None` means "no opinion", not "fine".

### The agent does not operate its own levers

Syscity's settings, pairing and display pages are the controls that govern the
agent. A task that taps through them is not doing what it was asked, and there is
no answer a person could give that would make it right — so a write whose app is
Syscity's own (`takeover::is_own_app`, the package and anything under it) is
refused before dispatch, without a question. Reads stay allowed: the agent may
look at its own screen to report on it.

### Two displays, one app

Two displays on one phone are two places to type, and each has its own focus —
but the app on them is one app: one process, one account, one set of unsaved
edits. So writes are locked twice:

- **per display**, not per device: two agent displays do not wait for each other,
  because they do not share a focus;
- **per `(serial, package)`**, because they do share the app.

The app is only known after the screen is read, so the order is: read, then take
the app's lease, and check a completion counter taken *before* the read. If
another writer finished in between, the lease reports `stale` and the call
re-reads — refusing instead if the app itself changed, since that would be a
second lease on a different app while holding the first.

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

### Naming an element that will still be there

An index is a position in one *reading* of the screen. A row arrives, the list
scrolls, and "element 7" is something else by the time the tap is dispatched —
the tap then lands on the wrong control and reports success. Every element
therefore carries a `key` built from what identifies it (resource id, text,
content description, class) and not from where it sits, and every read carries
a `snapshot` naming that enumeration.

`android_input` accepts either:

| Reference | What it survives |
|---|---|
| `target` (an index) | nothing — it is checked against the live read and refused if the element is gone, changed or not clickable (`target_description` makes the check exact) |
| `key` + optional `snapshot` | the list moving: the element is looked up again in the live tree. `reference_moved: true` in the result says the screen had changed since the read |

A key that finds nothing, or finds several elements that are indistinguishable
from each other, is refused as `STALE_REF` with the candidates listed. The
alternative — tapping the first of two identical rows — is exactly the silent
wrong-target failure the key exists to prevent. `android_ui_tree` and
`android_observe` return both fields.

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

### A page drawn in JavaScript (`android_web`)

A WebView node in the accessibility tree says "WebView" and nothing else: the
page's text lives in the page, not in a native view. So a login form rendered by
JavaScript is invisible to `android_ui_tree`, `android_query`, the extractor and
OCR alike — they all read the same empty node. `android_web` opens the page in a
WebView Syscity owns and asks the page itself:

```text
android_web action=open url=https://example.com/login  → page (title, text, controls)
android_web action=read
android_web action=type  index=1 value=alice
android_web action=type  index=2 value=…
android_web action=click index=3
android_web action=script script="document.querySelector('#err').textContent"
```

A read numbers the controls it found (`1..n`) and a click or type addresses that
numbering, so the stale-`ref` rule is the same one the accessibility path has: a
navigation or a re-render makes the numbers mean something else, and an index
that is no longer in the fresh read is refused rather than pressed. A press is
still a press — the label is classified, asking a human when one is reachable,
and counted by the repetition ladder — and `open` walks the same SSRF guard as
`web_fetch`.

The browser is the agent's, with its own cookies: it has no session from the
user's browser, so a site that needs a login starts logged out. `close` discards
even those, and the next `open` starts over.

The page half of the protocol is one script, `src/computer/platform/mobile/web/page.js`,
carried in the Rust binary and sent with `open`; the Kotlin side re-injects it
after every page load, since a navigation replaces the document and with it every
function the previous one had.

**Why not CDP.** The desktop browser stack is `chromiumoxide` behind the
`browser` feature, which the `mobile` profile leaves off — turning it on adds
twenty crates — and CDP reaches a WebView only when the app is debuggable
(`setWebContentsDebuggingEnabled`, then a `localabstract:webview_devtools_remote_<pid>`
forward). Release APKs, the build that runs unattended, have no CDP endpoint at
all. Evaluating a small script in the page works in every build and needs no new
dependency.

The tool only exists inside the phone app, because only that process can create a
`WebView` cheaply. A desktop driving a remote phone is told so and pointed back at
the accessibility tree.

### A second screen (`android_display`)

A task can run on its own display, so the user keeps the phone. The agent drives
it with `android_display`: `start` creates it and returns an id, `launch` opens
an app on it, `frame` returns a PNG, `tap`/`swipe`/`text`/`key` act on it, `stop`
tears it down. `mute`/`sound` silence an app and give it its voice back. Three
more answer questions the model would otherwise have to guess at:

- **`limits`** — how many displays may be open at once and why not more (API 34
  for per-display focus, a low-memory phone, a display that already failed to
  create).
- **`paste`** — the clipboard route for non-ASCII text. `text` goes through the
  device's key character map and drops or mangles CJK; a paste carries whatever
  the app can render, and the helper puts the user's own clipboard back
  afterwards — but only if it could read it first. If it could not, nothing is
  pressed at all (`CLIPBOARD_UNREADABLE`): the text never reached the clipboard,
  so a Ctrl+V would insert whatever the person had copied themselves.
- **`status`/`restart`** — whether the helper is answering, and replacing a
  stale one that outlived the app that started it.

The same route covers the user's own screen. `android_input action: "text"` with
non-ASCII text tries, in order: ADBKeyboard (when it is the input method), the
phone's own helper, and only then the error that used to be the first answer. The
helper is what makes the middle step possible at all: it runs as uid 2000, can
inject the Ctrl+V pair onto display 0, and can read and restore the person's
clipboard — an ordinary app can do none of the three. There the mode is *append*,
never replace: a replace opens with Ctrl+A, which would also wipe whatever they
had already typed into the field.

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

`scripts/android-audio-check.sh` does the same for the sound: it opens a display,
checks the app is silent and recorded, closes it and checks the mode is back,
checks that a `sound` with nothing recorded writes nothing, hands the app to the
user and back, and finally kills the helper with an app silenced to prove the
record outlives the process and the next helper restores it.

An app the agent opened on its second screen is an app the user cannot see.
Whatever it plays — a notification chime, a video started by a stray tap, a voice
note — comes out of a phone showing something else, with nothing on the screen to
connect it to. So `launch` silences the app it starts, and `mute`/`sound` do the
same for an app that arrived some other way. The switch is the per-package
`PLAY_AUDIO` app-op, which only the shell-identity helper can set, and the rules
that make it safe to do to somebody's phone are in `AudioMute.kt`:

- **the prior mode is read before the write, and written back on the way out** —
  `sound` means "what it was", not "audio on". `appops get` prints a package line
  (only when set) above a uid line (always); reading the first line would call a
  silenced package untouched, so the package line wins.
- **only recorded packages are ever written.** A refused read is a refused mute:
  an app whose mode cannot be established is left alone, and `mute` says so
  rather than guessing.
- **the record survives a kill.** It lives in `/data/local/tmp`, owned by the
  shell uid — not in `java.io.tmpdir`, which under `app_process` is a tmpfs and
  a reboot would take the record with it. A new helper restores whatever the last
  one left silent at start-up, and `releaseAll` does it at teardown. The same app
  on two displays is counted, not re-recorded.
- **a record names the helper that wrote it** (`owner <pid> <starttime>`) and a
  newcomer leaves a live helper's record alone. Two helpers can run at once — the
  app decides one is missing and starts another — and restoring the first one's
  record would make an app the user cannot see audible again while its display is
  still up. The start time pins the process, because a pid alone is reused.
- **a mute that does not take is not fatal.** The app is already running and
  merely audible; `mute` reports `muted: false` with `AUDIO_UNCHANGED` instead of
  failing the launch.

### One display, one writer

Two conversations can be told to drive the same phone at the same time, and a
tap lands under whatever the other one is doing. Two mechanisms keep them apart,
and neither is sufficient alone:

- **`TargetLocks` on the host**, keyed by serial (`android-device:<serial>`), so
  a call waits only for another call on *that* phone. A call with no pinned serial
  takes the kind-wide lock, because it cannot name the device adb will pick.
  Display-addressed writes narrow this to the display, and take a separate
  `(serial, package)` lease on the app — see *Two displays, one app*.
- **A hold on the device**, set by `giveToUser` when `android_hand_to_user` hands
  the display to a person and cleared by `takeBack` when the tool takes it back.
  Every write on the helper checks it; reads do not, so the waiting session can
  still judge what the person did. The hold carries a deadline as a backstop: a
  caller that died mid-handoff would otherwise leave the display unwritable for
  the rest of the session.

The device-side hold is what makes a handoff *exclusive* rather than merely
serialized. The host lock only covers sessions on this machine; a second session
that connected while a handoff was in flight, or one on another host, takes a
different lock or no lock at all. A refused write comes back as
`DISPLAY_HELD_BY_USER` — a different code from `DISPLAY_HELPER_ERROR` — because
"somebody is using it" is an instruction to wait, and "it did not work" is an
instruction to retry.

`android_hand_to_user` deliberately holds no `TargetLocks` guard while waiting
for the person: doing so would block the phone for as long as they take, which is
the opposite of what the handoff is for.

A handoff is also a place where people type instead of pressing a button. The
question is open for minutes; a message sent to the agent during that time would
otherwise sit in the `ExecutionController`'s steer queue — which the tool loop
drains only *between* iterations — and reach the model after the handoff's own
result. It would read as a remark about the previous step rather than about the
one the person was in the middle of. So the wait watches the controller's steer
notify as well as the answer, drains what arrives, and leaves it on the tool
context as a **note** (`ToolContext::note` / `ExecutionController::note`). The
loop attaches notes to the result of the tool that waited, before the result goes
into the transcript, so the reply reads in the right order:

```
User said "Done"; the screen changed while they had it.

[Sent while this step was waiting on you; it came after the question was
answered:]
- actually, use my work account
```

A steer that arrives when *no* tool is waiting is untouched: it stays in the
queue and is delivered the usual way, at the top of the next iteration.

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
- **`PLAY_AUDIO` is per package, not per display.** An app the agent opened is
  silent everywhere, including a second copy the user has open themselves —
  which is why it is only ever muted while it sits on the agent's display.

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
