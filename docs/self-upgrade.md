# Syscity Self-Upgrade Architecture

## Overview

Syscity can modify its own source code, compile, replace its binary, and restart — all triggered through chat conversation, without relying on external process managers (systemd/launchd/tmux).

Separately, released binaries can be updated in place from GitHub Releases without touching source — see [Flow D: Online Self-Update](#flow-d-online-self-update-from-github-releases).

## Architecture: Shim + Core

```
User: "syscity start"
  |
  v
Shim (~/.syscity/bin/syscityd)          # user-facing command (supervisor)
  |-- Check ~/.syscity/bin/syscity exists
  |-- Record binary mtime, spawn syscity and wait
  |
  |-- Exit code 42 + marker + mtime changed -> restart (upgrade)
  |-- Exit code 99  -> propagate and exit (full shutdown)
  |-- Any other exit -> check mtime changed -> restart (dev restart)
  |                    (with crash protection: rapid exit delays longer)
  |
  v
Core (~/.syscity/bin/syscity)           # replaceable core
  |-- Source at ~/.syscity/src/ (git-managed, or symlinked to dev tree)
  |-- Runs Gateway + all existing features
  |-- Can modify its own source -> cargo build --release
  |-- Replaces ~/.syscity/bin/syscity
  |-- Graceful exit with code 42 -> Shim restarts new version
```

## Components

### 1. Shim (`src/bin/shim.rs`, binary `syscityd`)

Minimal supervisor binary with **strategy B+C** hybrid behavior:
- Always restart the core unless explicitly told to shut down
- Three exit modes: upgrade (42), full shutdown (99), auto-restart (everything else)

| Exit Code | Behavior |
|-----------|----------|
| 42 | Upgrade mode — check marker + mtime, then restart |
| 99 | Full shutdown — propagate and exit syscityd |
| Other | Auto-restart — if binary mtime changed, restart immediately; if not, apply crash backoff |

```rust
const UPGRADE_EXIT_CODE: i32 = 42;
const SHUTDOWN_EXIT_CODE: i32 = 99;
const UPGRADE_MARKER: &str = ".upgrade-pending";
const MIN_RESTART_INTERVAL_MS: u64 = 5000;

fn main() {
    let syscity_dir = dirs::syscity_dir();
    let core_path = syscity_dir.join("bin/syscity");
    let mut last_mtime = get_mtime(&core_path);
    let mut last_restart = std::time::Instant::now();

    loop {
        let start_mtime = get_mtime(&core_path);
        let status = Command::new(&core_path)
            .args(std::env::args().skip(1))
            .status()
            .expect("Failed to start syscity");

        match status.code() {
            Some(UPGRADE_EXIT_CODE) => {
                // Scheme C: dual detection — marker file + binary mtime
                let marker_path = syscity_dir.join(UPGRADE_MARKER);
                let new_mtime = get_mtime(&core_path);
                let marker_exists = marker_path.exists();
                let mtime_changed = new_mtime != last_mtime;

                if marker_exists && mtime_changed {
                    eprintln!("Syscity upgraded, restarting...");
                    let _ = std::fs::remove_file(&marker_path);
                    last_mtime = new_mtime;
                    std::thread::sleep(Duration::from_secs(1));
                    last_restart = std::time::Instant::now();
                    continue;
                }
                // Partial upgrade signal — log and exit
                eprintln!("Upgrade signal incomplete, exiting.");
                std::process::exit(1);
            }
            Some(SHUTDOWN_EXIT_CODE) => {
                eprintln!("Syscity shutting down.");
                std::process::exit(0);
            }
            Some(code) | None => {
                // Strategy B+C: auto-restart if binary changed, crash-backoff otherwise
                let new_mtime = get_mtime(&core_path);
                let elapsed = last_restart.elapsed().as_millis() as u64;

                if new_mtime != start_mtime {
                    eprintln!("Binary changed, restarting...");
                    std::thread::sleep(Duration::from_millis(500));
                    last_restart = std::time::Instant::now();
                    continue;
                }

                // Crash protection: if core exited too fast, wait longer
                let delay = if elapsed < MIN_RESTART_INTERVAL_MS {
                    std::cmp::max(5, MIN_RESTART_INTERVAL_MS.saturating_sub(elapsed) / 1000)
                } else {
                    1
                };
                eprintln!("Syscity exited ({}), restarting in {}s...", code.unwrap_or(-1), delay);
                std::thread::sleep(Duration::from_secs(delay));
                last_restart = std::time::Instant::now();
                continue;
            }
        }
    }
}

fn get_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}
```

### 2. Core (`src/main.rs`, binary `syscity`)

The existing Syscity application, built as `syscity` binary. Includes:
- All existing Gateway/WebSocket/Agent functionality
- New `SelfUpgradeTool` for self-modification
- Modified `daemon.rs` to support exit code 42 on upgrade

### 3. SelfUpgradeTool (`src/tools/self_upgrade_tool.rs`)

Agent-facing tool for self-modification:

| Action | Description |
|--------|-------------|
| `status` | Show git branch, commit, uncommitted changes, build status |
| `patch` | Apply code modification via file_edit wrapper |
| `build` | Run `cargo test --lib` then `cargo build --release` |
| `upgrade` | Replace binary, write upgrade marker, graceful exit (code 42) |
| `rollback` | `git reset --hard HEAD~1`, rebuild |

**Parameters:**
```json
{
  "type": "object",
  "properties": {
    "action": {
      "type": "string",
      "enum": ["status", "patch", "build", "upgrade", "rollback"]
    },
    "file_path": { "type": "string" },
    "old_string": { "type": "string" },
    "new_string": { "type": "string" }
  },
  "required": ["action"]
}
```

### 4. Directory Layout

```
~/.syscity/
├── bin/
│   ├── syscityd             # Shim (user-facing command, never replaced)
│   └── syscity              # Core (replaceable)
├── src/                   # Syscity source code (git repo)
│   ├── .git/
│   ├── Cargo.toml
│   ├── src/
│   └── ...
├── agents/
├── cron/
├── logs/
└── syscity.toml
```

## Upgrade Flows

### Flow A: Self-Upgrade via Chat (Syscity modifies itself)

```
User: "Syscity, fix the heartbeat routing bug"

Agent:
1. file_read -> src/heartbeat/runner.rs
2. file_edit -> apply fix
3. shell -> cargo test --lib          (must pass)
4. shell -> cargo build --release      (compile)
5. shell -> cp target/release/syscity ~/.syscity/bin/syscity
6. SelfUpgradeTool upgrade:
      - git commit -am "auto: fix heartbeat routing"
      - write ~/.syscity/.upgrade-pending marker
      - Gateway graceful shutdown
      - exit(42)

Shim:
1. Detect exit code 42
2. Check ~/.syscity/.upgrade-pending exists
3. Verify ~/.syscity/bin/syscity mtime changed since last spawn
4. Remove marker file
5. Wait 1 second
6. Re-spawn ~/.syscity/bin/syscity
7. New version starts, Gateway boots

User: sees "Syscity upgraded, restarting..."
```

### Flow B: External Development via Claude Code

```
Developer: Claude Code, fix the heartbeat routing bug

Claude Code:
1. Edit src/heartbeat/runner.rs in dev tree
2. cargo test --lib                    (must pass)
3. cargo build --release
4. cp target/release/syscity ~/.syscity/bin/syscity

Shim (strategy B+C):
1. Core exits (or next check loop detects mtime change)
2. Binary mtime changed -> restart immediately (500ms delay)
3. Re-spawn ~/.syscity/bin/syscity
4. New version starts, Gateway boots

Developer: Web UI reconnects automatically
```

### Flow C: Full Shutdown

```
User: "syscityd stop"  (or sends SIGUSR1 to syscityd)

Core:
- Receives shutdown signal
- Graceful shutdown Gateway
- exit(99)

Shim:
- Detect exit code 99
- Propagate and exit
- User returns to shell
```

### Flow D: Online Self-Update (from GitHub Releases)

The binary-update path: no source, no local build — download a published
release, verify it, swap the binary, restart. Shared core lives in
`src/update/`:

| Piece | File | Role |
|-------|------|------|
| Discovery | `src/update/github.rs` | Query the GitHub API for the latest `lightconsen/syscity` release, semver-compare against the running version |
| Download | `src/update/download.rs` | Fetch the platform tarball and verify it against the `.sha256` published alongside the release — nothing is applied unverified |
| Apply | `src/update/apply.rs` | Extract the binary, write a temp file next to the current executable, atomically rename over it (a crash mid-apply never leaves a truncated binary) |
| Platform | `src/update/platform.rs` | Map os/arch to the release asset name; platforms without published binaries are rejected up front |

Surfaces:

- **CLI** — `syscity update` checks, downloads, verifies, installs, then
  restarts the daemon; `syscity update check` only reports; `--json` for
  machine-readable output, `--no-restart` to skip the daemon restart.
- **Daemon** — `GET /api/v1/update/status` (cached 6h; `?refresh=1` forces a
  check), `POST /api/v1/update` (runs download/apply in the background,
  `202 Accepted`), `GET /api/v1/update/progress` (phase/percent). The daemon
  restarts itself via a detached `syscity restart --pid <old>` helper that
  waits for the old process to exit before starting the new binary. The
  endpoints are registered in `build_router` in `src/gateway/lifecycle/router.rs` and governed by
  `[update]` in the config.
- **Desktop** — the Tauri updater plugin with minisign-signed bundles
  (pubkey in `desktop/tauri.conf.json`; the private key lives in the
  `TAURI_SIGNING_PRIVATE_KEY` GitHub secret). A `check_for_updates` command
  and tray item drive it. A gateway constructed embedded (desktop) defers:
  `POST /api/v1/update` refuses and points at the desktop updater.
- **Web UI** — a global update banner plus a Settings → Update section with
  progress polling; inside the desktop webview it routes to the Tauri
  updater.

Config (`~/.syscity/syscity.toml`):

```toml
[update]
enabled = true     # master switch for the update endpoints / CLI path
auto_check = true  # background release check at daemon startup
```

Release pipeline (`.github/workflows/release.yml`): CLI tarballs each ship a
`.sha256`; desktop builds publish minisign-signed updater artifacts
(`.app.tar.gz` / installers + `.sig`) and a per-platform `latest.json`
rewritten with absolute download URLs.

## Safety Mechanisms

| Layer | Protection |
|-------|------------|
| Compile gate | `cargo test` must pass before upgrade |
| Git snapshot | Auto-commit before every upgrade |
| Rollback | `self_upgrade rollback` -> git reset + rebuild |
| Shim protection | SelfUpgradeTool blocks modification of `src/bin/shim.rs` |
| Sandbox | `file_write` cannot overwrite shim binary |
| Binary atomic | `cp` new binary, then exit — no partial writes |
| Dual detection (Scheme C) | Exit 42 + marker file + mtime check prevents false restarts |
| Download verification (Flow D) | SHA-256 checksum checked before any apply; desktop bundles minisign-signed |

## Exit Code Semantics

| Code | Meaning |
|------|---------|
| 0 | Normal shutdown |
| 1 | Generic error |
| 2 | Config error |
| 3 | Validation error |
| 4 | Not found |
| 5 | External service error |
| **42** | **Upgrade complete, restart required** |
| **99** | **Full shutdown — syscityd exits too** |

## Development Mode (Mode 1)

When using external tools like Claude Code for development, you work in the original source tree (`~/work/syscity/`) and deploy to `~/.syscity/bin/syscity`. The Shim's strategy B+C makes this seamless:

```bash
# In your dev directory
cargo build --release
cp target/release/syscity ~/.syscity/bin/syscity
# syscityd detects mtime change and auto-restarts syscity core
```

### Graceful vs Forceful Restart

| Command | What happens | Use case |
|---------|-------------|----------|
| `cp target/release/syscity ~/.syscity/bin/syscity` | Binary mtime changes → syscityd restarts core with 500ms delay | Normal dev iteration |
| `kill -TERM <syscity_pid>` | Core exits with code 0 → syscityd restarts with 1s delay | Trigger graceful restart |
| `kill -KILL <syscity_pid>` | Core dies → syscityd restarts with crash backoff | Force restart |
| `syscityd stop` (or `kill -USR1`) | Core receives signal → exits 99 → syscityd exits | Full shutdown |

### dev-restart.sh

```bash
#!/bin/bash
# scripts/dev-restart.sh — One-command dev restart
set -e

cargo build --release
cp target/release/syscity "$HOME/.syscity/bin/syscity"
echo "Binary deployed. syscityd will auto-restart syscity core."
```

### Source Sync Strategies

When using Mode 1 (external development), `~/.syscity/src/` can be managed in three ways:

| Strategy | How | Pros | Cons |
|----------|-----|------|------|
| **A: Independent copy** | `cp -r ~/work/syscity ~/.syscity/src` | Dev and runtime fully isolated | Manual sync needed |
| **B: Symlink** | `ln -s ~/work/syscity ~/.syscity/src` | Single source of truth | `target/` pollutes dev tree |
| **C: Git worktree** | `git worktree add ~/.syscity/src self-upgrade` | Shared git history, clean separation | Requires git setup |

**Recommendation**: Use **Strategy A** for safety (Syscity self-upgrade only touches its own copy), or **Strategy C** if you want to track auto-commits in your main git history.

## Scheme C: Dual Detection

Why dual detection? A core process could exit with code 42 for reasons other than a real upgrade (bug, manual test, unexpected panic during shutdown). Scheme C requires **all three** signals to be present:

1. **Exit code 42** — core claims upgrade intent
2. **Marker file `~/.syscity/.upgrade-pending`** — core explicitly wrote upgrade intent to disk
3. **Binary mtime changed** — a new binary was actually copied into place

If any signal is missing, the Shim treats it as a normal error and exits. This prevents infinite restart loops from spurious exit 42.

## Daemon.rs Modifications

After Gateway shutdown:
```rust
if should_upgrade {
    let _ = tokio::fs::remove_file(&pid_file).await;
    // Write upgrade marker so Shim knows this is a real upgrade
    let marker = crate::dirs::syscity_dir().join(".upgrade-pending");
    let _ = tokio::fs::write(&marker, "").await;
    eprintln!("Upgrade complete, restarting...");
    std::process::exit(42);
}
```

## Cargo.toml Modifications

Add second binary target:
```toml
[[bin]]
name = "syscityd"
path = "src/bin/shim.rs"

[[bin]]
name = "syscity"
path = "src/main.rs"
```

## Open Questions

1. **Source provisioning**: How does `~/.syscity/src/` get populated initially?
   - Option A: First run copies current source tree
   - Option B: Clone from GitHub
   - Option C: User manually places source

2. **Build environment**: Does the user machine have `cargo` + `rustc`?
   - Option A: Require Rust toolchain (dev environment)
   - Option B: Provide pre-compiled fallback

3. **Scope of modification**: Can agent modify any Rust source?
   - Option A: Any file (full self-improvement)
   - Option B: Whitelisted modules only
   - Option C: Plugin/module extensions only

## Files to Create

| File | Lines | Purpose |
|------|-------|---------|
| `src/bin/shim.rs` | ~70 | Minimal supervisor with dual detection |
| `src/tools/self_upgrade_tool.rs` | ~300 | Patch/build/upgrade/rollback |
| `scripts/install-shim.sh` | ~30 | Install shim as `~/.syscity/bin/syscityd` |
| `scripts/dev-restart.sh` | ~10 | One-command dev build + deploy + restart |

## Files to Modify

| File | Changes |
|------|---------|
| `Cargo.toml` | Add `[[bin]]` targets (`syscityd`, `syscity`) |
| `src/main.rs` | Adjust entry point |
| `src/daemon.rs` | Support exit codes 42 (upgrade) and 99 (shutdown) + write upgrade marker |
| `src/gateway/mod.rs` | Graceful shutdown signal |
| `src/tools/mod.rs` | Register SelfUpgradeTool |
