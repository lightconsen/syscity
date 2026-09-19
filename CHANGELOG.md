# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release workflow integration: the `## [X.Y.Z]` section matching a pushed tag
becomes the GitHub Release body. Write release notes here BEFORE tagging;
if no section matches, the release falls back to auto-generated notes.

## [Unreleased]

## [0.3.7] - 2026-09-20

### Highlights

- **The TUI runs inline.** `syscity tui` writes the conversation into the terminal's own scrollback as it arrives and redraws only a small live region at the bottom — composer, status row, blocking prompts. Native scrolling, text selection and copy keep working, and the transcript outlives the client. It is also no longer a one-line-at-a-time thing: output streams and freezes line by line, tool calls and results appear as they happen, approvals and `ask_user` questions take over the bottom of the screen rather than the whole of it, a message typed mid-turn is queued instead of run alongside, and a dropped socket is reconciled against the gateway's history on reconnect.
- **Browser control can actually interact.** Key combinations, right/middle/double/triple clicks, and drags that move; a screenshot now states the coordinate space it is in and refuses clicks outside it; `escalate` is an explicit way to say the page is not enough.
- **A pass over every trust boundary.** The gateway trades a long-lived token for a single-use, 30-second upgrade ticket and keeps credentials out of its own log lines; every event carries an audience so one client is not handed another's traffic; scopes are granted from the credential rather than from the request; webhook deliveries are bounded and replayed signatures refused; CORS no longer mirrors the request origin.

### Added

- The TUI's status row doubles as a run indicator: an animated frame, a rotating word, and a parenthetical that carries the real information — elapsed time and whether the turn is thinking, responding, or inside a named tool call. It all goes away when the turn ends.
- `/history [n]` reprints a window of the conversation in reading order, and `/history more` pages backwards from the oldest message shown using the gateway's `before` cursor — so a session longer than the 100 messages a resume loads is now walkable back to its beginning.
- The composer lists the matching slash commands while a `/command` is being typed, windowed around the current `Tab` selection.
- `syscity tui` can run against a piped stdin/stdout as a line protocol: one line in, one turn out, slash commands included. Prompts that need a human are settled rather than left to time out.
- `feat(browser)`: key combinations with the macOS command that makes them deliverable; right, middle, double and triple clicks; drags that move the pointer.
- `feat(android)`: a stable code on a failed UI dump, an explicit check for whether the screen shows what was expected, and an observation that still hands over the screenshot when the UI tree could not be read.
- `feat(tools)`: `requires_approval` now gates a call where there is someone to ask; tools declare whether they can be safely retried; tools that drive one shared target are serialized against each other.
- `feat(skills)`: `/learn` authors a skill from a source or from this session, and writes where the watcher is already looking.
- A `...` menu on agent rows in the sidebar — rename, delete, settings.
- Observed token estimates are recorded beside the provider's own count, so the two can be compared.

### Changed

- One place decides which model a fresh install uses, instead of the default being restated wherever it was needed.
- Authentication no longer treats cookies as a credential.
- The TUI's tool output is one transcript line per row and is capped by a row budget: the ellipsis is the last row of the budget, not an extra one. Live previews get five argument rows (leaving room for the `⚙ tool` header in a six-row region); a reprint from history gets eight.
- The web client refuses a connection rather than putting the gateway token in the URL when the ticket exchange fails for a reason other than the gateway not having one.

### Fixed

- The TUI padded every wide character in scrollback with a blank column, which made Chinese text look sparse and made lines wrap sooner than they should.
- The composer wrapped its input to the full width while the first row also carried the `> ` prompt, so an input that filled the row exactly lost its last character and the cursor clamped onto the one before it.
- A control chord that was not bound typed its bare letter — `Ctrl+U` put a `u` in the draft. `Ctrl+Alt` is still typed, because that is AltGr on several layouts.
- A multi-line tool call or result was packed into a single transcript line; the renderer drops a newline, so it came out as one run-together row, on screen and in scrollback.
- In line mode, a streamed answer was printed twice, and the line read from stdin was never the line sent.
- A slash command that failed ended the whole TUI; it is now a line of output.
- An approval the gateway refused was dropped from the queue, leaving the blocked turn with no way to be unblocked.
- The run state did not converge when the connection dropped: the status row could spin "running" forever and a prompt could keep the keyboard. In-flight requests now fail as soon as the socket dies rather than after their own timeout.
- Events were applied without checking which session they belonged to.
- A keystroke typed while a command was in flight was dropped instead of queued.
- Gateway calls held the state lock across the round trip, and commands ran inside the event loop, so a slow gateway froze input and redraw.
- The panic hook was left installed after the TUI exited, and a signal left the terminal in raw mode.
- The TUI reported a failed send as a successful one.
- An approval is now routed to the session that raised it, not to whichever client asks next, and an approval whose waiting client is gone is denied rather than left to expire.
- A provider stream that failed mid-flight was dropped; it now has a channel and is retried before any output has been shown.
- Compaction could separate a tool call from its result; the pair is now kept together.
- `audit security --format json|yaml` emitted something other than what it says; the daemon endpoint is resolved instead of assuming `18080`; the `config.toml` the daemon writes now parses and takes effect.
- Outbound replies could be cut inside a multi-byte character.
- A tripped cost guard now clears itself, and can be cleared.
- Browser: keys are pressed in a form the browser recognises rather than as script-dispatched events a page can tell are fake; a script that reports an error is no longer counted as a successful action; the action shapes the schema advertises are accepted.
- Computer-use verification that could not run was being counted as one that passed.
- A spaceless script is no longer classified as a "very short message".
- Shutdown lets in-flight writes land before closing the pool.
- The released notes are drafted from this file; a failure to draft them is no longer silent.
- `rustls` is bumped to the release that fixes RUSTSEC-2026-0285, and the web client's shipped and build dependency trees are clear of advisories.

## [0.3.6] - 2026-09-14

### Added
- Android observation via `android_observe`: returns the numbered actionable UI elements and a screenshot from one call, reports the pixel size of each, and warns when the screenshot and the UI tree describe different screens.
- `android_input` can tap by element index using `target`; the target is re-read from the live screen and the tap is refused if the element moved, is no longer clickable, is disabled, or does not match `target_description`.
- `android_input` supports `sequence` for dispatching multiple coordinate taps in a single adb command, useful for controls that auto-fade before the next turn.
- Android text input now escapes device-shell metacharacters, turns newlines into Enter key events, and types non-ASCII via ADBKeyboard when active; otherwise it fails with actionable guidance instead of silently mangling the text.
- `android_ui_tree` now returns a numbered list of actionable elements with class, labels, center, bounds, and clickability/enabled state, rather than raw XML.

### Fixed
- The `device` argument is now honored by `android_screenshot`, `android_observe`, `android_input`, `android_app_manager`, and `android_ui_tree`; each result reports the serial actually acted on, and invalid device values are refused rather than silently falling back.
- Android screenshots are now returned as valid PNG bytes instead of being corrupted by text encoding.
- Android UI dump failures now classify the likely cause (device offline, unsettled screen, secure window, disabled accessibility service) and point to the screenshot fallback for continued work.
- Empty Android UI trees now explain that the screen is likely a game, canvas, or secure surface and recommend using the screenshot, rather than reporting an unexplained failure.
- Fixed matching for the common adb error `device 'emulator-5554' not found`.
- Tracked scripts and generated Android build files no longer leak machine-specific absolute paths.
- Removed vulnerable dependencies by replacing `serde_yml` with `serde_norway`, upgrading `ratatui` to 0.30, and bumping `event-listener` to a patched release.

## [0.3.5] - 2026-09-11

### Added
- Cloud models are discovered from the cloud proxy's `GET /v1/models` instead of a hardcoded list, refreshed lazily (10-minute TTL) when the model picker opens. The picker shows the models the proxy actually serves and drops entries it no longer advertises.
- The model picker lists **Cloud** and **Local** sections independently. A model id served by both (e.g. `deepseek-v4-pro` through the cloud proxy and from a directly-configured provider) now appears in both, and the cloud copy is independently selectable and routes through the proxy.

### Changed
- Authenticated sessions and device pairings are persisted to the shared SQLite store, so login state and paired devices survive a restart. Only domain-separated SHA-256 token digests are written — never plaintext.
- Audit records attribute actions to the real user (and the rate limiter is keyed per user) instead of hardcoded placeholder actors.
- The secret store and filesystem layout are per-instance handles threaded through the gateway rather than process globals. Single-instance behaviour is unchanged — same `~/.syscity` root, same 0600 encrypted store.

### Fixed
- macOS downloads no longer trigger Gatekeeper's "cannot verify the developer" warning: CLI tarballs and desktop builds are Developer ID signed and notarized.
- `system.reload` no longer removes the runtime cloud provider.
- The status bar shows the bare model name for cloud-pinned sessions instead of the internal `cloud/<id>` reference.

## [0.3.4] - 2026-09-10

### Added
- Composer **+** picker with Experts, Skills, and Connectors tabs. Experts is searchable and single-select; Skills and Connectors are multi-select. In sessions already bound to a specific agent, the Experts tab is hidden and the active tab falls back to Skills.
- Attached expert and skill chips are sent with the next message as hidden per-turn context that guides skill loading and expert delegation without being saved in the transcript. Responses are cached separately for different chip selections.
- Skills and Connectors tabs now show a search box once their list exceeds 10 items. Search matches names and descriptions, and each tab has its own no-match row. Switching tabs clears the search.
- Connector chips are enabled automatically on send when they are not already enabled. If enabling fails, the message still sends and a toast shows the failure.

### Changed
- In Marketplace, an expert’s Summon button now keeps its primary style after the expert is installed.

### Fixed
- The chat view now stays pinned to the bottom while message scrolling settles, so late message measurements don’t leave it short of the newest message.
- Markdown messages no longer rebuild from scratch on unrelated re-renders, reducing flicker and scroll jumps.
- Gzip archive member splitting is now exact instead of scanning for magic byte sequences, fixing flaky archive index rebuilds when compressed data contained gzip-header-like bytes.

## [0.3.3] - 2026-09-09

### Added
- Added full English/Chinese localization across the web UI — chat, settings, Knowledge Base, Workspace, onboarding, ask/approval, updates, and Extensions — plus a settings language switcher.
- Added cloud credit controls: account popover with balance badge, daily check-in, one-time signup bonus, invite code redemption, purchasable packs, and recent ledger. A low-balance banner now appears when credit is running out.
- Added usage and credit reporting to completed chat turns when the provider supplies it, and cloud credit exhaustion now surfaces as an `insufficient_credits` error.
- Added per-model credit multiplier labels in the model picker and a cloud-first model selector with vendor logos.
- Added skill and expert package installation from the marketplace, with toast feedback, starter prompt prefill, and live installed/connected/error state.
- Added in-memory SWR caches for the Extensions catalog and Knowledge Base pages.

### Changed
- The web accent palette switched to Syscity indigo.
- Marketplace catalog language now follows the active UI language; switching languages re-syncs instead of serving stale mixed translations.
- Model router provider selection is deterministic across restarts and prefers direct non-cloud providers for duplicated model IDs.
- The model router now honors each provider's configured default model for calls that do not specify a model.
- Multi-query retrieval can be pinned to a specific provider/model and now falls back to the original query when expansion fails.

### Fixed
- Fixed `/sw.js` returning HTTP 500.
- Fixed cached-but-empty marketplace catalogs being served indefinitely; they now self-heal on the next fetch.
- Fixed in-place marketplace upgrades wiping already-cached package contents.
- Fixed failed health probes from extending circuit breaker cooldown and locking providers out after the cooldown expired.
- Fixed collection-filtered sqlite-vec KNN search by using a subquery.
- Fixed cloud provider requests being sent without the `/v1` prefix, which caused 404s and circuit breaker trips.

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
