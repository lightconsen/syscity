# Syscity Mobile Migration Plan

Status: **Implemented** (P1–P4 landed 2026-08; this document is the original
plan, kept for design rationale) · Target: Android first, iOS second · Owner: TBD

> Landing commits: P1/P2 bring-up (`9985e50` — `mobile` feature profile,
> platform gating, Android shell), P3 execution layer (`cb23c73` —
> `is_available()` branches, `ProcessRunner` abstraction, WASM `code_exec`,
> MCP in-process/HTTP), P4 mobile-native (`a64f444`, `652a9f5`, `3232008`,
> `153af2c`, `8ad2d69` — device bridge + capability/SAF tools, WorkManager
> cron wake + loopback ADB self-pairing, pairing UI, iOS build + DevicePlugin,
> Shortcuts/App Intents bus). Later hardening: the iOS Swift link in
> `desktop/build.rs` is gated behind
> `#[cfg(target_os = "macos")]` because `tauri_utils::build::link_apple_library`
> is macOS-host-only — iOS targets build only from a macOS host.

This document is the migration plan for running **Syscity** standalone on a
mobile device — the full runtime on-device: gateway, agents, delegation, memory,
SQLite. It covers a feasibility assessment of the current codebase, the platform
constraints that shape the design, the adaptation layers required, and a phased
roadmap with acceptance criteria.

---

## 1. Goal

Run **Syscity** standalone on a mobile device: gateway, agents, delegation,
memory, and SQLite all on the phone. Strategy: **Android first, iOS second**.

- **Effort:** the phased roadmap in §5.
- **Feasibility:** possible, with a scoped-down tool surface. The agent "brain"
  (LLM API calls) is already remote over standard HTTPS, which is the core
  value and is fully portable.
- **Key asymmetry:** Android can spawn same-UID subprocesses; **iOS cannot
  fork/exec at all**. Standalone iOS is limited to in-process libraries (§4).

---

## 2. Feasibility: what the codebase gives us today

### 2.1 The core is mostly portable

The top-level `Cargo.toml` has **no GUI dependencies** (no GTK/X11/objc/tauri in
the core crate). Most of the stack is portable, but a handful of dependencies
need mobile-specific handling (SQLite linking, `notify`, `keyring`, and the
default feature set as a whole) — see §2.2.

| Component | Dep | Mobile |
|---|---|---|
| HTTP/WS gateway | axum + tokio | ✅ portable |
| Concurrency | tokio (tasks/threads) | ✅ portable |
| Storage | sqlx + SQLite | ⚠️ needs `sqlite/bundled` (links system libsqlite3 today) |
| Agent loop, context, tools | — | ✅ portable |
| Delegation tree | in-process `SubagentRegistry` | ✅ portable |
| LLM provider calls | HTTPS | ✅ (needs network + key) |
| Web UI | React SPA | ✅ (webview/PWA) |

> **Default features are not mobile-safe.** The default feature set pulls in
> heavy native/ML dependencies (`llama-cpp-2`, `ort`, `chromiumoxide`,
> `sqlite-vec` + `libsqlite3-sys`, plus all channel integrations). Mobile
> builds must start from `--no-default-features` and opt back in — see §4.9
> and P1 task 1.1.

The desktop shell is a separate crate (`desktop/`) that **embeds the gateway
in-process** via `use syscity::` and serves it to the Tauri WebView at
`http://127.0.0.1:<port>` (`desktop/src/lib.rs`). This embedding pattern is
exactly what Tauri mobile needs — it is the mobile path's starting point.

### 2.2 Hard blockers (must be addressed)

1. **`src/computer/` is desktop-only.**
   `src/computer/mod.rs:42-46` gates platform modules with
   `#[cfg(target_os = "linux"/"macos"/"windows")]` and depends on
   X11/Wayland/CoreGraphics/Win32. It must be cfg-excluded on mobile.
2. **`~/.syscity` base directory is hardcoded.**
   `src/dirs.rs:34` returns `home_dir()/.syscity` with no env override.
   Android has no `~`; every path in the app derives from this single point.
3. **Subprocess-spawning tools.**
   `shell.rs`, `code_exec.rs`, `process.rs`, `pdf.rs`, `image.rs`, `tts.rs`,
   `patch.rs`, `nodes.rs`, `src/security/tailscale.rs`, and
   `src/mcp/client.rs` (`connect_stdio`) all `Command::new` external binaries
   that do not exist on a phone (§4).
4. **Desktop-specific execution model assumptions.**
   `fs_watch`, cron background wake, and unrestricted filesystem access are
   desktop assumptions that Android/iOS restrict (§4).
5. **Dependency-level blockers not visible in source review.**
   - **SQLite is not bundled** — `libsqlite3-sys` links the system library
     (`cc`/`pkg-config`/`vcpkg` in `Cargo.lock`); Android/iOS have no linkable
     system SQLite. Enable `sqlite/bundled`.
   - **`notify` has no native iOS backend** — backends are linux+android
     (inotify), macOS (FSEvents), BSD/kqueue, windows. It is used at six call
     sites (`config/hot_reload.rs`, `config/watch.rs`, `cli/kb.rs`,
     `rag/ingestion/watch.rs`, `skills/watcher.rs`, `computer/fs_watch.rs`),
     not just the `fs_watch` tool. notify ships a `PollWatcher` fallback that
     compiles everywhere, so iOS may simply degrade to polling — verify in P1;
     if the fallback does not engage, cfg-exclude or poll manually.
   - **`keyring` (opt-in) is incompatible with mobile** — its `apple-native` /
     `windows-native` / `secret-service` backends do not cover iOS/Android.
     Mobile builds must not enable `--features keyring`; the default 0600
     encrypted file store works in the sandbox short-term, with Android
     Keystore / iOS Keychain as the long-term backend.
6. **The default feature set is desktop-shaped.**
   `default = [...]` includes `local-embeddings` (llama-cpp-2 → cross-compiles
   llama.cpp C++), `vision` (ort → ONNX Runtime ships no prebuilt binaries for
   android/ios targets), `browser` (chromiumoxide), and `sqlite-vec` (direct
   `libsqlite3-sys` dependency that bypasses sqlx). None of these build
   out-of-the-box on mobile. Mobile builds must define a pruned feature
   profile (P1 task 1.1) rather than build with default features.

### 2.3 Existing mechanisms we reuse (do not reinvent)

- **`is_available()`** — `Tool` trait method (`src/tools/types.rs:769`),
  already implemented per tool (`shell.rs:330`, `delegate_tool/tool.rs`,
  `gateway.rs:296`, `planner.rs:80`, `cron_tool.rs:392`, `acp_tool.rs:265`,
  `computer.rs:139`). This is the platform capability probe: add a
  `#[cfg(target_os = "...")]` branch and the agent loop automatically skips
  unavailable tools.
- **MCP transport already has three modes** — stdio, SSE, streamable-HTTP
  (`src/mcp/client.rs:3`). Standalone mobile switches MCP servers to HTTP, or
  uses an in-process channel (§5 P3).
- **Delegation is process-internal** — child agents are tokio tasks keyed by
  `delegation:<run_id>`, not subprocesses. The entire delegation tree
  (collaboration, handoff, recursion, shared `task_state`) works on mobile
  unchanged.
- **Unified SQLite** — `data/` already holds `syscity.db` + `delegations.db`.
  DB files live fine inside the app sandbox; no storage migration needed.
- **`SYSCITY_HOME` single choke point** — everything derives from `dirs.rs`,
  so one configurable base relocates the whole tree (see P1).
- **WASM runtime already in the tree** — `wasmtime` + `wasmtime-wasi` back the
  plugin system (`src/plugins`). The §4.5 embedded `code_exec` engine is not
  net-new, and wasmtime's Android/iOS build is an early cross-compile canary.

---

## 3. Platform constraints that shape the design

### 3.1 Android

| Capability | Constraint |
|---|---|
| Subprocess spawn | Allowed for same-UID binaries — but **only from `nativeLibraryDir`**: SELinux blocks `exec` from the app-private data dir (`filesDir`) for targetSdk 29+. Cannot run other apps' processes. |
| Shell | `/system/bin/sh` (mksh) + `toybox` (busybox-like) exist. Runs with the app's sandboxed UID; no root, no cross-app access. |
| Bundled binaries | Native executables must be shipped in the APK (`jniLibs` with `extractNativeLibs=true`) and exec'd from `nativeLibraryDir`. **No runtime download-then-exec** — the binary set is fixed at build time. |
| Filesystem | App-private sandbox (`filesDir`/`cacheDir`); user files via Storage Access Framework (`content://`). |
| Background | Process killed under Doze/App-Standby; need `WorkManager`/`JobScheduler` for deferred work. |
| Watch | `inotify` works inside the app sandbox only. |

### 3.2 iOS

| Capability | Constraint |
|---|---|
| Subprocess spawn | **Prohibited.** The sandbox blocks `fork`/`exec` for app code. No shell, no bundled executable. |
| Filesystem | App sandbox only (`NSDocumentDirectory` etc.). |
| Background | Suspended in background; deferred work via `BGTaskScheduler`. |
| Consequence | **In-process libraries are the only execution path.** `shell`/`process`/`code_exec`-as-subprocess are impossible; embedded interpreters/WASM are the replacement. |

### 3.3 Concurrency is not the problem

Syscity's concurrency is already thread/task-based (tokio), not process-based.
Threads give parallelism and shared-memory messaging on every platform. What
threads **cannot** provide is:

- an interpreter that does not exist on the device (threads don't conjure
  `python`), and
- fault isolation. (A `panic!` in a tokio task is actually contained by its
  `JoinHandle` under `panic=unwind`; the real process-killer is UB in native
  libraries reached via FFI, which on mobile takes the UI down with it.)

The mobile answer for isolation is the **WASM sandbox** (`wasmtime` memory/
time/fs limits), which is stricter than a desktop subprocess. Threads + WASM
together are the mobile replacement for subprocesses.

---

## 4. The adaptation layers

### 4.1 Configurable base directory (P1)

Add an env override in `src/dirs.rs`:

```
SYSCITY_HOME  (default: <home>/.syscity  — unchanged on desktop)
```

Every path function in `dirs.rs` already derives from `syscity_dir()`, so this
one change relocates `data/`, `logs/`, `skills/`, `agents/*/workspace/`,
`delegations/*/`, `artifacts/` together. The mobile host sets it at startup:
- Android: Kotlin `context.filesDir`
- iOS: `NSDocumentDirectory`

### 4.2 Platform-gate `src/computer/` (P1)

Extend the existing `#[cfg(target_os = ...)]` gates so the **desktop-control
parts** compile out for `android`/`ios`: the X11/Wayland/macOS/Windows
adapters, `use_loop`, `vision`, screen/mouse/keyboard tools. Those have no
mobile semantics; they are replaced by mobile-native capabilities in P4.

**Keep `platform/mobile/` compiled in.** Its tool sets are platform-agnostic
(`target_os: []`) and §4.10 retargets them at the host device — blanket-
excluding the whole `src/computer/` module would delete the P4.5
differentiator along with the desktop code.

### 4.3 `ProcessRunner` abstraction (P3)

Collect the scattered `Command::new` sites in `src/tools/*` behind a trait:

```rust
trait ProcessRunner {
    fn run(&self, argv: &[&str]) -> Result<CommandOutput, Error>;
}
```

| Target | Implementation |
|---|---|
| Desktop | `std::process` (today's behavior) |
| Android | `sh`/`toybox` for the whitelisted set; bundled native binaries exec'd from `nativeLibraryDir` (`filesDir` exec is SELinux-blocked); error otherwise |
| iOS | No process execution; in-process library path or error |

### 4.4 `is_available()` platform branches (P3)

Add `#[cfg]`-based branches to the existing `is_available()` implementations so
the agent loop never offers a tool that cannot run:

| Tool | Android | iOS |
|---|---|---|
| `shell` | ✅ `sh`/toybox, sandboxed UID | ❌ |
| `process` (ps/kill) | ❌ (own process only) | ❌ |
| `code_exec` | WASM / embedded interpreter | WASM / embedded interpreter |
| `pdf`/`image`/`tts` | in-process Rust crates / Android TTS | in-process Rust crates / iOS TTS |
| `computer` (desktop control) | ❌ | ❌ |
| `mcp` stdio servers | in-process or HTTP | in-process or HTTP |
| `fs_watch` | sandbox-scoped `inotify` | sandbox-scoped or polling |

### 4.5 Execution engines (P3)

`code_exec` replaces subprocess interpreters with **in-process engines**,
selected by cargo feature so desktop keeps its subprocess behavior:

- JavaScript → `rquickjs` / `boa` (pure Rust, single-threaded VM; parallelize
  with a thread pool of instances)
- Python → `rustpython` (pure Rust)
- Untrusted/arbitrary → `wasmtime` + WASI (sandboxed memory/fs/time; already
  in the tree for the plugin system)

**iOS note:** iOS prohibits JIT — `mmap(PROT_EXEC)` requires the
`dynamic-codesigning` entitlement, which normal apps cannot get. wasmtime on
iOS therefore runs through the **Pulley interpreter** (or AOT-precompiled
modules), not the Cranelift JIT. The same constraint applies to the WASM
plugin system on iOS. All three engine paths above are pure-Rust/interpreted,
so `code_exec` remains in scope for iOS.

### 4.6 MCP transport (P3)

- Servers that support it → `streamable-http` (client already supports it).
- Pure-Rust MCP servers → compile into the app and connect over an in-process
  channel (`tokio::mpsc`) instead of stdio pipes.
- Node-based servers → not viable standalone; disable on mobile.

### 4.7 Run survival (P2, core) and background scheduling (P4, optional)

**Keeping in-flight runs alive (P2).** An agent's value is long, multi-step
turns — and those are exactly what mobile process lifecycles kill:

- **Android:** when the user switches away mid-run, the process becomes
  killable under Doze / memory pressure. While a run is active the app should
  hold a **foreground service** (persistent notification, `dataSync` /
  `specialUse` type) so minutes-long LLM/tool loops are not killed mid-turn.
- **iOS:** backgrounding suspends the app almost immediately; an in-flight
  LLM request freezes and has usually timed out by resume. Agent turns must
  be **retryable/resumable** (persist run state; treat a frozen request as a
  retry, not a failure) and the UI should surface "run interrupted" instead
  of hanging silently.

**Background scheduling (P4, optional).** Cron/heartbeat scheduling is
in-process (`src/cron/cron/scheduler.rs`, a tokio timer) and runs while the
app is foregrounded. For wake-in-background:

- Android: `WorkManager`/`JobScheduler` via a Tauri/native plugin
- iOS: `BGTaskScheduler`

Background wake is an enhancement, not a blocker — standalone mobile can ship
with foreground-only scheduling first.

### 4.8 User-file access (P4)

- Keep agent file operations inside the app sandbox (workspaces, delegations,
  artifacts) — no migration needed.
- User-facing files use the existing HTTP surface (`/api/v1/artifacts/*path`)
  plus an Android `content://` (SAF) adapter on `file_read`/`file_write` where
  real user-file access is required.

### 4.9 Dependency & feature exclusions (build-time)

Mobile builds must handle these dependencies explicitly:

| Dependency / feature | Why | Handling |
|---|---|---|
| **default features** | desktop-shaped: channels, `local-embeddings`, `vision`, `browser`, `plugins`, `hot-reload`, `sqlite-vec` | build with `--no-default-features` + an explicit `mobile` profile; opt back in per-platform |
| `local-embeddings` (llama-cpp-2) | cross-compiling llama.cpp C++ is heavy, and FFI UB kills the whole app process on mobile | exclude from the mobile profile; use remote embeddings |
| `vision` (ort) | ONNX Runtime ships no prebuilt binaries for android/ios targets | exclude from the mobile profile |
| `sqlite/bundled` | no linkable system SQLite on Android/iOS | enable the feature |
| `sqlite-vec` + `libsqlite3-sys` | direct C dependency bypassing sqlx; also links SQLite | enable `libsqlite3-sys/bundled` alongside `sqlite/bundled`, or drop `sqlite-vec` from the mobile profile |
| `keyring` feature | backends cover macOS/Windows/Linux only | never enable on mobile; file store short-term, Keystore/Keychain later |
| `chromiumoxide` (optional) | browser automation, desktop-only | keep off |
| `tailscale` pairing | spawns `tailscale` CLI, reads `/var/run/tailscale/...` socket | cfg-gate off / mark unavailable |
| `notify` watchers (6 call sites) | no iOS backend | Android: inotify (sandbox-scoped); iOS: polling fallback or disabled |
| `nix` signal/kill paths | iOS support partial | cfg-gate `daemon.rs` + `process.rs`; verify all four in-tree nix versions compile |
| daemon mode | double-fork/setsid/PID, Unix desktop concept | mobile always runs `--foreground` |
| `dirs` crate | `home_dir()` unreliable on Android | always resolve via `SYSCITY_HOME` |

### 4.10 On-device automation (Android)

Once Syscity runs on the phone, the existing `platform/mobile/` tool surface
(`android_input`, `android_screenshot`, `android_app_manager`,
`android_ui_tree`) can be retargeted at the *host* device — turning "Syscity
controls a phone" from a desktop-bridge feature into the app's
differentiator. Two paths, in increasing order of privilege:

1. **AccessibilityService bridge.** Declare an accessibility service; after
   the user grants it, the app can read the UI tree and dispatch gestures
   in-process (the Tasker/MacroDroid model). No ADB, no pairing. Caveats:
   Google Play scrutinizes accessibility use by non-assistive apps (see §6),
   and the implementation lives on the **Kotlin side** (manifest declaration +
   `AccessibilityService` subclass) bridged to Rust via a Tauri plugin/JNI —
   this is a fresh implementation behind the existing tool schema, not a
   reuse of the ADB-spawning Rust tools.
2. **Loopback ADB (Shizuku/LADB pattern).** Pair wireless debugging against
   the device itself (`adb connect localhost`), then the existing ADB tools
   work nearly unchanged with the device pointed at self. Grants shell-UID
   privileges (input injection, screencap, `pm`/`am`) — strictly more than
   the app UID. Caveat: wireless debugging resets on reboot (and on network
   change on some OEM builds), so pairing is **per-boot on most devices**,
   not one-time.

The agent-facing tool schema is identical either way; only the transport,
privilege level, and implementation side (Kotlin bridge vs. reused Rust ADB
tools) differ.

### 4.11 iOS automation bus (Shortcuts / App Intents)

Arbitrary UI automation is **not possible** for an App Store iOS app: reading
another app's UI tree, injecting input, and screenshotting other apps are all
private-API-only (WebDriverAgent works solely on developer-signed builds).
The supported control plane is Apple's intent layer — "the brain is local,
the hands are Shortcuts":

- **Shortcuts as the executor.** Run any shortcut via
  `shortcuts://run-shortcut?name=...&input=...` (a de facto, undocumented but
  stable URL scheme — App-Store-safe since it is just an `openURL`); hundreds
  of built-in actions (iMessage, Focus modes, HomeKit, calendar/reminders,
  HTTP requests, clipboard) plus whatever third-party apps expose through
  **App Intents** (iOS 16+). Results return via the `x-callback-url`
  convention, an App Group shared container, or a callback to the app's own
  URL scheme.
  **Key UX constraint:** every `shortcuts://` invocation is a *visible
  foreground handoff* — there is no headless execution, and third-party App
  Intents can only be invoked through system mediation (Shortcuts/Siri), not
  directly. The design consequence: **batch actions into a few "fat"
  shortcuts** so one handoff performs a whole chain.
- **Syscity exposes its own App Intents** so Siri/Shortcuts can drive the
  agent (voice entry point, automation triggers: time, location, NFC).
- **ReplayKit broadcast** gives a read-only view of the screen, explicitly
  started by the user from Control Center — usable for a "watch and advise"
  copilot, never for input.
- **MDM** covers supervised/enterprise devices (app install, restrictions) —
  a separate product surface, noted for completeness.

This maps cleanly onto the existing approval-gated tool model: every
Shortcuts action is user-visible, auditable, and revocable — a smaller reach
than Android's pixel-level automation, with a much cleaner trust story.

### 4.12 Mobile UI: reuse the web SPA (do not rewrite)

The existing `web/` SPA (React 19 + Vite + Tailwind 4, served by the gateway)
is already on the right architecture for mobile. Three pre-existing designs
make it directly reusable:

- **The Tauri bridge is already written** — `web/src/SyscityWebSocketTransport.ts`
  detects `__TAURI__`, resolves the gateway via the `get_api_url` command,
  and waits for the `gateway-ready` event before connecting the WebSocket.
  Tauri 2 mobile exposes the identical JS API, so this works unchanged in the
  Android/iOS WebViews.
- **All Tauri coupling is guarded** — every `@tauri-apps/api` use is a
  dynamic import behind an `isTauri` check, so browser/PWA contexts never
  load it.
- **`vite-plugin-pwa` is already in devDependencies** — the installable-PWA
  fallback path is pre-scaffolded.
- **Asset delivery needs the `embedded-assets` feature.** Without it the
  gateway falls back to reading `dist/` from a relative filesystem path,
  which does not exist inside a mobile app bundle (no controllable CWD). The
  mobile profile (P1 task 1.1) therefore includes `embedded-assets` so the
  SPA is compiled into the binary via rust-embed — and CI must run
  `pnpm -C web build` *before* the Rust build.

The chat core (`@assistant-ui/react`: message stream, markdown, streaming)
is platform-agnostic, as is the WebSocket + localStorage session transport.

What remains is an **adaptation pass, not a rewrite** — the app currently has
almost no responsive breakpoints (3 total) and a desktop three-pane layout
(fixed sidebar / chat / draggable document-preview split):

| Area | Today | Mobile treatment |
|---|---|---|
| Sidebar | fixed left column | drawer / bottom sheet |
| Chat ↔ preview split | horizontal flex ratio | full-screen overlay or tab switch |
| CommandPalette | keyboard-first (⌘K) | touch entry point |
| Touch targets | desktop hover affordances | ≥44pt targets, long-press menus, hover degradation |
| Virtual keyboard | unhandled | `visualViewport` handling, input docked above keyboard |
| Safe areas | unhandled | `env(safe-area-inset-*)` for notch / home indicator |
| SettingsPanel | multi-pane | single column |

Mobile-only UI to add: the permission-onboarding flows (camera / location /
accessibility / wireless-debugging pairing), gateway-token injection into the
WebView (P2 task 2.5), and status surfaces for the P4 native capabilities.

Rough estimate: 70-80% of `web/` carries over untouched; the rest is
additive Tailwind work (`sm:` classes) plus the mobile-only additions. A
native rewrite (SwiftUI/Compose) is not justified — markdown/streaming
rendering is exactly what the web stack is best at.

---

## 5. Phased roadmap

> **Desktop non-regression invariant:** every phase's acceptance includes the
> desktop build and full test suite (`./scripts/self-check.sh`) staying green
> on linux/macos/windows, with the desktop shell behaving unchanged. Nearly
> all mobile work is additive (new features, `cfg(target_os)` gates,
> mobile-host files); the exceptions that touch shared code are P2.5 (gateway
> auth — mobile-scoped only; desktop keeps `auth_mode = "none"`), P2.6 (web
> responsive pass — additive breakpoints only), P2.7 (retryable turns — shared
> `src/agent/` semantics), and above all **P3.2 (ProcessRunner), the
> highest-risk item: a pure behavior-preserving refactor on desktop** (env,
> cwd, pipes, timeouts, error mapping all unchanged), covered by the existing
> test suite.

### Phase 1 — Cross-compilation proof (Android)

Goal: prove the core compiles and runs on Android.

| # | Task | File(s) |
|---|---|---|
| 1.1 | Define the `mobile` feature profile: `--no-default-features` + explicit opt-ins (webchat, `plugins` (wasmtime cross-compile canary, §2.3), `embedded-assets` (SPA compiled into the binary, §4.12), `sqlite/bundled`, `libsqlite3-sys/bundled`); excludes llama-cpp-2, ort, chromiumoxide, keyring, channels | `Cargo.toml` |
| 1.2 | `SYSCITY_HOME` env override | `src/dirs.rs` |
| 1.3 | cfg-exclude the desktop-control parts of `src/computer/` on mobile (keep `platform/mobile/`, §4.2) | `src/computer/mod.rs` |
| 1.4 | Rust `aarch64-linux-android` toolchain + build target wired | `Cargo.toml`, CI |
| 1.5 | Headless smoke test: gateway starts, SQLite opens under sandbox path, HTTP responds | test harness |

**Acceptance:** `cargo build --target aarch64-linux-android
--no-default-features --features <mobile-profile>` succeeds; the binary
starts on Android and serves `/health`.

**Decision gate:** P1 acceptance requires the **dependency checklist** to pass
on `aarch64-linux-android` (and, for iOS later, `aarch64-apple-ios` +
`aarch64-apple-ios-sim` for the simulator dev loop):

- [ ] the mobile feature profile builds — no llama-cpp-2 / ort / chromiumoxide / keyring in the dependency graph
- [ ] SQLite links via `sqlite/bundled` (+ `libsqlite3-sys/bundled` if `sqlite-vec` is kept)
- [ ] `notify` compiles (Android: inotify; iOS: `PollWatcher` fallback, else cfg-excluded + manual polling)
- [ ] build without `--features keyring`
- [ ] `nix` (versions 0.26/0.27/0.28/0.29 in tree) compiles on the target
- [ ] `wasmtime` compiles on the target (iOS: Pulley interpreter path, no JIT)
- [ ] `src/computer/` excluded compiles clean

If any row fails, stop and reassess scope before proceeding.

### Phase 2 — Shell + UI on device

Goal: the core product (chat, delegation, memory) works on the phone.

| # | Task | File(s) |
|---|---|---|
| 2.1 | Tauri mobile init (android) or PWA/webview wrapper | `desktop/` |
| 2.2 | Web UI served by the embedded gateway in the app | `desktop/src/lib.rs` (already embeds gateway) |
| 2.3 | Set `SYSCITY_HOME` from `context.filesDir` at startup | mobile host |
| 2.4 | Allow cleartext HTTP to `127.0.0.1` for the WebView (targetSdk 28+ blocks cleartext by default) | Android `networkSecurityConfig` / manifest |
| 2.5 | **Enforce gateway auth on mobile builds** (mobile builds only — desktop keeps `auth_mode = "none"`; the transport change must keep the no-token path working). Loopback is *not* per-app isolated: any installed app with `INTERNET` permission can reach `127.0.0.1:<port>` and drive the agent. Generate a per-install token at first launch and inject it into the WebView via a Tauri command; never accept `AuthMode::None` on mobile | `src/gateway/`, mobile host |
| 2.6 | Mobile adaptation pass on the web SPA: responsive breakpoints, drawer sidebar, touch targets, virtual-keyboard + safe-area handling (§4.12). Reuse, not rewrite — the Tauri bridge and transport already work on mobile. **Additive breakpoints only; desktop layout unchanged** | `web/` |
| 2.7 | **Run survival:** Android foreground service (persistent notification) while an agent run is active; iOS retryable/resumable turns + "run interrupted" UX (§4.7) | mobile host, `src/agent/` |

**Acceptance:** installable Android app where the user can chat, spawn
delegations (orchestration / collaboration / handoff / recursion), and see
task_state + artifacts.

### Phase 3 — Execution-layer adaptation

Goal: bring the subprocess-dependent tools up (or down) correctly on mobile.

| # | Task | File(s) |
|---|---|---|
| 3.1 | `is_available()` platform branches | `src/tools/*.rs` |
| 3.2 | `ProcessRunner` abstraction + Android `sh`/bundled-binary impl. Desktop impl = today's `std::process` behavior, byte-for-byte (pure refactor, covered by existing tests) | `src/tools/*` |
| 3.3 | `code_exec` → WASM / embedded engines behind feature flags | `src/tools/code_exec.rs`, `Cargo.toml` |
| 3.4 | MCP: HTTP transport for supported servers; in-process channel for Rust libs | `src/mcp/client.rs` |
| 3.5 | `fs_watch` sandbox-scoped / polling; process tools disabled | `src/computer/fs_watch.rs`, `src/tools/process.rs` |

**Acceptance:** the agent can execute code (WASM) and connect MCP servers
(HTTP/in-process) on-device; unavailable tools are invisible to the agent.

### Phase 4 — Mobile-native capabilities (differentiator)

Goal: turn "Syscity on a phone" into "a phone-native Syscity".

| # | Task | Notes |
|---|---|---|
| 4.1 | Camera, geolocation, notifications, haptics, sensors via Tauri plugins | desktop lacks these |
| 4.2 | SAF/`content://` adapter for user-file access | Android |
| 4.3 | Background wake: `WorkManager` / `BGTaskScheduler` for cron/heartbeat (§4.7) | optional enhancement |
| 4.4 | iOS build (scoped-down tool surface) | post-Android |
| 4.5 | On-device automation: AccessibilityService bridge and/or loopback ADB; retarget the existing `platform/mobile/` tool surface at the host device (§4.10) | Android; the differentiator |
| 4.6 | Shortcuts/App Intents bus: run shortcuts, `x-callback-url` results, App Group channel; expose syscity's own App Intents (§4.11) | iOS; replaces UI automation |

---

## 6. Risk register

| Risk | Impact | Mitigation |
|---|---|---|
| Hidden platform-bound dependency breaks cross-compile | Blocked build | Mobile feature profile (P1 task 1.1) prunes known offenders up front; P1 dependency checklist (§5) gates the build; revisit scope on any failure |
| Mobile secret storage diverges (keyring is desktop-only) | API keys exposed | file store (0600) short-term; Keystore/Keychain backend later (§4.9) |
| iOS tool surface too small to be useful | Product value | Ship Android-first; note the §4.5 in-process engines (JS/Python/WASM via Pulley) are pure-Rust/interpreted and also work on iOS, so `code_exec` stays in scope once P3 lands |
| App-process crash from a misbehaving library thread | Lost state | WASM sandbox for untrusted code; keep desktop subprocess model on desktop |
| Background scheduling complexity | Scope creep | Ship foreground-only cron first; background is opt-in (4.3) |
| Process death mid-run (Android Doze/memory pressure; iOS instant suspension) kills long agent turns | Core-value failure: runs die silently | Foreground service while a run is active + retryable/resumable turns (P2 task 2.7); persist run state (§4.7) |
| User-file access semantics diverge from desktop | Confusion | Keep agent file ops sandboxed; user files via HTTP/SAF adapters (4.8) |
| Agent "workspace_only" / sandbox config interacts with sandboxed mobile FS | Security regressions | Re-verify `is_available()` + workspace confinement on device |
| Loopback gateway is reachable by any other installed app (loopback is not per-app isolated on Android/iOS) | Full agent takeover by a malicious app (read workspace, invoke tools, spend API keys) | Mandatory per-install auth token on mobile builds (P2 task 2.5); never ship `AuthMode::None` on mobile |
| Google Play rejects/restricts AccessibilityService use by a non-assistive app | App-store blocker for §4.10 path 1 | Declare the use case honestly in review; keep loopback ADB (path 2) as the no-policy-risk fallback |

---

## 7. References

- Base dir choke point: `src/dirs.rs` (`syscity_dir()` at line 34)
- `is_available()` trait method: `src/tools/types.rs:769`
- `src/computer/` platform gates: `src/computer/mod.rs:42-46`
- MCP multi-transport: `src/mcp/client.rs` (stdio / SSE / streamable-HTTP)
- In-process cron scheduler: `src/cron/cron/scheduler.rs`
- Desktop shell embeds gateway: `desktop/src/lib.rs`
- Desktop bundle targets (no mobile): `desktop/tauri.conf.json`
- SPA serving: `build_router` in `src/gateway/lifecycle/router.rs` (`/assets/*path` route)
- Web UI Tauri bridge (mobile-ready): `web/src/SyscityWebSocketTransport.ts`
  (`__TAURI__` detection, `get_api_url`, `gateway-ready` event)
- Delegation is process-internal: `src/agent/subagent_registry.rs`,
  `src/delegation/` (tasks keyed `delegation:<run_id>`)
- Existing mobile device bridges (retargeted at the host in §4.10):
  `src/computer/platform/mobile/` (Android ADB / iOS libimobiledevice)

---

*This document was written as a proposal; Phases 1–4 have since landed (see
the status header). Phase 1 was the smallest useful slice that proved the
whole idea.*
