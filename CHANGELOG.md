# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release workflow integration: the `## [X.Y.Z]` section matching a pushed tag
becomes the GitHub Release body. Write release notes here BEFORE tagging;
if no section matches, the release falls back to auto-generated notes.

## [Unreleased]

## [0.3.2] - 2026-09-07

### Added

- feat(ui): titlebar session context strip + PanelLeft sidebar toggle
- feat(chat): defer agent session creation to first message
- feat(ui): sidebar nav highlighting by active view
- feat(kb): document preview in resizable right panel, Titlebar toggle
- feat(kb): cloud backups toolbar chip + popover, progressive page load
- feat(kb): merge KB page with per-file backup and document viewer
- feat(kb): kb.doc_content WS method for document preview
- feat(kb): allow local_gguf embeddings for the knowledge base
- feat(kb): Backup & Sync panel replaces cloud KB management
- feat(cloud): KB backup & sync — cloud.kb.docs/push/pull orchestration over WS
- feat(kb): flat local document list with Agent column
- feat(kb): Knowledge Base view — local per-agent collections + cloud KBs
- feat(ui): show cloud avatar in AccountButton; English cloud banner
- feat(ui): New Session opens a welcome page; session created on first message
- feat(ui): merge marketplace sidebar entries into one Extensions item
- feat(ui): account button in titlebar; split marketplace entry into three
- feat(ui): move workspace toggle from composer toolbar to the Titlebar
- feat(ui): move status dot and theme toggle into the Statusbar zone
- feat(ui): move sidebar header (logo + name + toggle) into the titlebar zone
- feat(ui): align titlebar identity with the sidebar's right edge
- feat(ui): 3-row shell with Titlebar and Statusbar
- feat(desktop): overlay titlebar config + platform command
- feat(release): LLM-drafted release notes via scripts/.env config
- feat(release): full release flow in release.sh — changelog, bump, tag

### Fixed

- fix(rag): enable embeddings in local GGUF context params
- fix(kb): drop duplicated count in backup/restore result notes
- fix(rag): scope vector delete_by_source to a collection
- fix(cloud): flatten /auth/me user identity in cloud.status/cloud.token
- fix(web): wait for WS connection before submitting the OAuth callback token
- fix(cloud): rewrite asset URLs to absolute on the OAuth callback route
- fix(cloud): repair OAuth login flow (console URL + callback route)
- fix(web): substitute {VERSION} in title for dev server and prod build

### Changed

- chore(web): rename app title to "Syscity Agent"
- style(ui): dedicated bg-rail token, left rail sits a step below page
- chore: ignore .e2e-tmp scratch directory
- style(web): tighten sidebar top action spacing
- style: fix ASCII diagram alignment in README
- refactor(ui): rename MarketplaceView to ExtensionsView
- style(ui): move workspace toggle to the far right of the Titlebar
- style(ui): workspace toggle icon → lucide PanelRight
- feat(ui): pane-following chrome colors for Titlebar and Statusbar
- chore(desktop): grant core window/event capabilities

## [0.3.1] - 2026-09-04

### Highlights

- **Cloud features on by default in shipped builds, with a `--nocloud` opt-out**: release artifacts (CLI + desktop) and `./scripts/build.sh` compile the `cloud` feature by default; `syscity start --nocloud` disables cloud at runtime (propagated through background start and self-update restarts). Source `cargo build` still contains no cloud code (§2.7).

### Added

- `/goal` durability: round-level checkpoint hardening (atomic writes, TaskRegistry drain on shutdown), mid-round resume (an in-flight round continues from its checkpointed messages instead of restarting), per-goal token accounting, terminal outcome write-back to the parent session, and a `syscity goal list/resume/cancel` CLI
- Eval: suite token cost reported in TrialResult/Suite Summary, auto-appended eval ledger (`evals/ledger.md`), release-gate iteration loop recipe, and machine-enforced gate integrity (pre-commit + CI checks against threshold tampering)

### Changed

- `./scripts/build.sh` defaults to cloud features; `--nocloud` opts out and the old `--cloud` flag is removed

### Fixed

- A `[cloud]` config section without an explicit `enabled` key silently disabled cloud (serde `bool` default); it now shares the env-aware on-by-default value
- CI: publish one-line installers to GitHub Pages

## [0.3.0] - 2026-09-02

### Highlights

- **Syscity Cloud platform** (default-off `cloud` feature): account sign-in with popup + callback, a marketplace to browse and install experts/skills/connectors, cloud model / search / knowledge-base providers, device binding with usage & subscription display, and cloud-provisioned connectors — all double-gated behind the feature.
- **Document authoring**: slides canvas pipeline (`write_report format=slides` with live preview and PPTX export), full `write_document` coverage via authored-HTML docx/xlsx slices, chart/diagram generation (`svg_to_png` + `generate_chart`) with image embedding, and a document-authoring skill.
- **Closed-loop harness / eval**: turn feedback buttons with an eval dashboard, layered scorer wired into governance-weighted verdict gates, compression quality gating, human-review sampling, online monitoring, and a governed regression suite (`badcase-run`) — plus online sampling, a low-retention compression gate, feedback ops aggregation, and N=1 online shadow replay.
- **CLI migrated to WebSocket RPC**: agents, cron, skills, providers, plugin, device, admin, and audit commands now speak WS instead of REST.
- **Desktop & remote**: remote gateway connection mode (desktop and mobile), reuse of an already-running gateway, an in-web tool approval UI (modal + sidebar badge), and a WeChat Official Account channel.

### Added

- WS methods: `agents.*` (default/memory/import/export/config), `audit.*`, `cron.*`, `skills.*`, `providers.fallback`, `plugins.reload_all`, `device.pairing`, `mcp`, `security gate/allowlist/status`
- Marketplace: expert summoning, connector WS/HTTP surface + events, MCP connector abstraction (catalog sync, lifecycle DSL, bundled skills, persistent state)
- Office: slides rich text, anchoring, gradients, PPTX export; docx/xlsx authored-HTML slices; svg_to_png + generate_chart tools; docx image embedding
- Cloud: login/token/status/logout endpoints, `cloud_kb` tool, cloud search provider, OpenAI-compatible cloud model provider
- Site: per-platform download buttons (macOS Apple Silicon / Intel split), tab-switched install commands, pricing (USD on EN / RMB on zh), a Syscity Cloud section, light demo GIF with EN/中文 switcher, favicons
- Windows installer (`install.ps1`), `--cloud` build flag
- Release: CHANGELOG-driven release notes with auto-notes fallback; release artifacts published to Cloudflare R2 and served via the `syscity-releases` worker

### Changed

- Sidebar account/login moved to the top and bottom rows; composer merges image/file into one attach button; marketplace gets a full-screen view
- Desktop: reuse an already-running gateway; remote gateway connection mode with unified frontend HTTP base
- Site hero install tabs, larger platform icons, enlarged mobile screenshots

### Fixed

- CI: dropped the stale `engine_metrics` field, gated wechatmp webhooks behind the feature, hoisted `routes` above `r2_buckets` in the releases worker config
- Bumped wasmtime 45.0 → 46.0 for RUSTSEC-2026-0269

## [0.2.2] - 2026-08-24

### Highlights

- **Kernel write fences on all three desktop platforms**: `workspace_only` command tools (shell / process / code_exec) now run inside macOS Seatbelt, Linux Landlock, and Windows AppContainer + Job Object sandboxes — the agent can no longer write outside the workspace, enforced by the OS kernel, fail-closed.
- **Fresh-context goal loops ("Ralph" mode)**: `/goal --fresh` runs each round in a brand-new seedless sub-agent with a validated `handoff` JSON contract; round notes are browsable in the workspace, and interrupted goals are suspended with a structured `blocked_reason` until `/goal resume`.
- **Human-in-the-loop `ask_user` tool** with a web modal — agents can ask blocking questions mid-turn.
- **Per-turn observability**: full-fidelity turn records under `~/.syscity/turns/` plus SQLite metrics and a `syscity observe {stats,list,show,export,prune}` CLI.
- **Online self-update** for CLI, daemon, and desktop (minisign-signed updater bundles; verify-then-apply with SHA-256).

### Added

- Claude-Code-compatible shell hooks (`~/.syscity/hooks.json`): PreToolUse / PostToolUse / UserPromptSubmit / Stop with deny/ask/block decisions, fail-open by design
- Runtime invariant registry: modules register their own checks, `syscity invariants [--json]` runs them all, enforced by static analysis
- Content-addressed attachment store (`~/.syscity/attachments/sha256/…`) with dedup and `observe prune` GC
- Report artifacts now live in each agent's own workspace, served at `/api/v1/artifacts/@<agent>/…`
- Agent workspace file browser in the web UI (`workspace.list` / `workspace.read`)
- `screen_ui_detect` tool (OmniParser) with automatic fallback when the a11y tree is empty
- Post-execute tool hooks with block-with-feedback
- Cron run digest appended to `workspace/cron-log.md`
- Durable compaction: boundaries persisted, tool pairs kept, overflow retries once
- Crash-recovery: orphaned in-flight tasks marked failed at startup; session repair with `TOOL_OUTCOME_UNKNOWN` sentinel
- Eval framework in CI: deterministic YAML validation on every push, nightly `ci_smoke` smoke run against a live LLM (non-blocking, opens a tracking issue on failure), manual `release_gate` before tagging

### Changed

- Todo tool is now whole-snapshot replace (`{"todos": [...]}`); plans clear automatically on each new turn
- File writes guarded by read-before-edit version tracking
- Oversized tool outputs spill to disk (path-aware exemptions), shell keeps output tails
- Current time moved out of the system prompt into a per-request state snapshot (keeps the prompt prefix byte-stable for KV-cache reuse)
- Secrets masked on all config surfaces; `config.set` uses revision CAS (`REVISION_CONFLICT` on stale writes)
- `web_fetch`: stable error codes, per-hop redirect revalidation, non-2xx returned as result
- Unix process spawns report the terminating signal and kill the whole process group
- Grounding & honesty rules in the default system prompt: cite only tool-result facts, surface source conflicts, label prior knowledge as unverified

### Fixed

- Windows builds: gated Unix-only code paths; verified continuously via the zig cross-compile harness
- LLM judge hardened for evals: JSON recovery from prose, format-correction retry, declared-dimension gating, normalized threshold keys
- E2E tests allocate ports dynamically (no more "Failed to bind gateway" flakes)
- Desktop release pipeline: macOS deployment target pinned for llama.cpp, updater signing key rotated, updater manifests generated for tauri v2, desktop bundles collected from the workspace-root target dir

## [0.1.2] - 2026-06-11

### Added

- Initial release of Syscity AI Assistant
- Core agent architecture with tool system
- SQLite persistence for sessions and memory
- Provider abstraction supporting OpenAI and Anthropic APIs
- CLI with interactive chat mode
- Web search and fetch tools
- File operations (read, write, edit, glob)
- Shell command execution
- Code execution with Python sandbox
- Todo/task management
- Session search with FTS5
- Dual memory architecture (procedural + user model)
- Context compression strategies
- Iteration budget management
- Autonomous skill creation with security guard
- Subagent delegation
- Persistent assistant spawning
- Assistant mesh for inter-assistant communication
- MCP (Model Context Protocol) integration
- Security module with auth, allowlist, and rate limiting
- Cron scheduler for recurring tasks
- Telegram channel integration
- Discord channel integration
- Slack channel integration
- Message formatting for all channels
- Docker deployment configuration
- Systemd service configuration
- Kubernetes manifests
- GitHub Actions CI/CD workflows
- Example skills (weather, news, calculator, todo, reminder)
- Comprehensive documentation

### Changed

- Relicense project from MIT to Apache-2.0

## [0.1.1] - 2026-06-04

### Changed

- Upgrade `sqlx` 0.7 -> 0.8.6 (security fix)
- Upgrade `wasmtime` 15 -> 45.0.0 (security fix)
- Remove `rustls-pemfile` and `rsa` from dependency tree

## [0.1.0] - 2024-01-01

### Added
- Initial project setup
- Basic structure and CI
