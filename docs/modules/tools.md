# Tools Module

Capabilities the AI assistant can use to interact with the world.

## Design

- **`Tool` trait** — `name()`, `description()`, `parameters_schema()`, `execute(args, context)`
- **`ToolRegistry`** — Registration, lookup, execution with caching, circuit breaker, and trust-level filtering
- **`ToolContext`** — Execution context grouped into `identity` (user/conversation/sender), `sandbox` (working dir, limits, workspace root, `agent_workspace`), and `model` (provider/model name, skill trust), plus optional `delegation` scope and `ask_queue` handles
- **`ToolRegistrar`** — Validation-aware registration with name, schema, and security validators
- **`ApprovalQueue`** — Human-in-the-loop approval system with risk levels

### Built-in Tools

| Category | Tools |
|----------|-------|
| File | `file_read`, `file_write`, `file_edit`, `glob` |
| Shell | `shell` (with allowlist) |
| Web | `web_fetch`, `web_search` |
| Code | `execute_code` (sandboxed) |
| Memory | `memory_search`, `memory_get` |
| Session | `sessions_list`, `sessions_history`, `sessions_send`, `sessions_yield`, `session_status` |
| Agent | `delegate`, `agents_list` |
| Interaction | `ask_user` (human-in-the-loop clarification) |
| Report | `write_report` |
| Browser | `browser` |
| Image | `image_generate` |
| PDF | `pdf` |
| Process | `process` |
| Grep | `grep` |
| Patch | `apply_patch` |
| Time | `time` |
| Todo | `todo` |
| Cron | `cron` |
| TTS | `tts` |
| Gateway | `gateway` |
| ACP | `acp_session`, `acp_spawn` |
| MCP | `mcp_connection` (auto-discovers MCP server tools) |
| Canvas | `canvas` |
| SDK | `sdk` |
| Nodes | `nodes` |
| STT | `stt` |
| Command Detection | `command_detector` |

### Notable Tool Behaviors

- **`todo`** — Whole-snapshot semantics: the input `{"todos": [...]}` IS the complete new task list and each call atomically replaces the stored state (last write wins; no partial merge). State lives in a `TodoState` shared by the tool, the registry, and the agent engine, persists to `~/.syscity/todos/<conversation_id>.json`, and is cleared automatically when a new user turn begins so the UI never shows a stale checklist.
- **`file_write` / `file_edit`** — Guarded by a read-before-edit `WriteGuard`: modifying an existing file the model has not read in this conversation (or that changed on disk since the last read) is rejected with corrective feedback. Successful writes record the new version, so read → edit → edit flows work without re-reading.
- **`shell`** — Output truncation keeps the head and the tail (command errors usually sit at the end). Results carry a `signal` field when the process was terminated by a signal, and every Unix child is spawned as its own process-group leader so timeouts/cancellation kill the whole group, not just the direct child.
- **`web_fetch`** — Selection failures carry stable machine-readable codes in `data.code`; redirects are followed manually (max 10 hops) with every hop re-validated against the SSRF navigation guard; non-2xx responses are returned as successful results (status + body) rather than tool errors.
- **`write_report`** — Reports land in the producing agent's own workspace at `<agent-workspace>/artifacts/` (falling back to the legacy global artifacts dir for custom workspace layouts) and return an owner-addressed URL for the frontend preview. With `format: "slides"` the content is canvas HTML — one `<div class="slide">` per slide (1280×720 px canvas, absolutely-positioned children, incl. `font-size`) — the preview renders pixel-exact, and `GET /api/v1/artifacts/...?to=pptx` converts it server-side into a real .pptx download (explicit font sizes are injected into the deck's run properties; unsized elements keep auto-fit). `format: "docx"` (flowing HTML) and `format: "xlsx"` (table HTML, one sheet per `<table>`) convert the same way via `?to=docx` / `?to=xlsx`.
- **`ask_user`** — Pauses the turn on an `AskQueue` oneshot until the human answers via the web modal (`ask.required` / `ask.respond` over WS). Background contexts (delegated sub-agents, goal runner, cron/heartbeat/standing orders) refuse with a clear message instead of blocking.
- **Screenshot producers** (`browser`, `computer`, `screen_state`) — Large image payloads are written once to the content-addressed attachment store at `~/.syscity/attachments/sha256/<2>/<rest>`; tool results carry a compact `{"type":"image_ref",...}` marker plus a human note instead of megabytes of base64. Current-turn refs are materialized back as image blocks at request time; older-turn refs degrade to a one-line placeholder. Unreferenced objects are swept by `syscity observe prune`.
- **Output spill** — Successful tool outputs above 32 KiB (configurable, `ToolRegistry::with_spill_threshold`) are written to `<workspace>/.syscity/spill/` and replaced with a head/tail preview plus a retrieval hint. The exemption is path-aware: only calls whose path-like argument resolves under the spill directory return full content, which breaks the read → spill → read loop without exempting every `file_read`.

- **`delegate`** — A child runs as the agent the delegation is made *for*. `target_agent` arrives in the model's own arguments, so it is default-deny: any other name is refused, logged, and ignored, and the delegation task row records the agent that actually ran rather than the string that was asked for. Cross-agent delegation would need an operator-configured allowlist; see [delegation-wake.md](../delegation-wake.md) §3.5.
- **MCP tools** — Registered as `mcp__{server}__{tool}` and reached through the registry from both the agent path and `mcp.call_tool`, so blocked/degraded prefixes, policy hooks, approval and the content filter apply to either. A name the registry cannot dispatch (server not connected, or beyond `max_tools`) falls back to a direct client call, guarded by the blocked list alone; see [mcp.md](mcp.md).

### Security Features

- **Path traversal detection** — `../`, `~`, null bytes blocked in SecurityValidator
- **Command injection detection** — `;`, `|`, `$`, `` ` ``, `$(` blocked
- **Sandbox mode** — Resource limits via `setrlimit` (Unix): memory, CPU, FDs, processes
- **Kernel write fences** — `workspace_only` command tools (`shell`, `execute_code`, `process`) are fenced at the kernel level, not just path-checked: Seatbelt on macOS, Landlock on Linux, AppContainer + Job object on Windows. The fence carries two further clauses: **protected paths** (`.git` and `.syscity` stay unwritable even inside a granted root — a `.git/hooks` payload runs on the next `git` command, which is the escape the carve-out closes; Seatbelt enforces this, Landlock grants are additive and cannot subtract, so Linux relies on the granted-root boundary alone) and **`[security] fence_network`** (default off: `curl`/`git fetch` are ordinary work; when on, macOS adds `(deny network*)`, Linux a seccomp socket filter, and Windows simply withholds the AppContainer network capability SIDs)
- **Workspace boundary** — `workspace_only` mode restricts file ops to `workspace_root`
- **Read-before-edit guard** — `file_write` / `file_edit` reject blind or stale writes to existing files (see `src/tools/write_guard.rs`)
- **Hooks** — Programmable gates before and after tool execution: pre-execute policy hooks can deny or route to approval; post-execute hooks can replace the output or block the result with feedback the model sees as an error. A Claude-Code-compatible shell hooks bridge (`~/.syscity/hooks.json`, fail-open) maps PreToolUse / PostToolUse / UserPromptSubmit / Stop events onto these points
- **Approval queue** — Human-in-the-loop for high-risk tools with `RiskLevel` classification. The announcement is scoped to the conversation that raised it (`PendingApproval.session_id` → `approval.required` routes to that session's subscribers; no conversation means broadcast). This scopes the *prompt*, not the *decision*: any client holding the `write` scope can still answer any pending approval — the premise is a single operator, and owner-checked decisions are deferred until a second identity can connect (see tui.md).
- **Permissions** — Claude-Code-style gating in front of every tool call (`src/tools/permissions.rs`, evaluated in `ToolRegistry`'s gate before hooks, locked order: deny rules → ask rules → allow rules → mode → hooks → `requires_approval` fallback). A `[permissions]` config block holds the gateway-wide default mode (`default` / `accept_edits` / `plan` / `bypass`), an `allow_bypass` flag (bypass is refused without it), and `allow`/`deny`/`ask` rules — each `"tool"` or `"tool:glob"` matched against the call's primary argument (shell→command, file tools→path, web_fetch→url). A session can override its mode (`sessions.set_mode`, ephemeral until restart; the TUI's `/mode` does this) and the status line shows a `⨿ plan`-style tag while a non-default mode is active. Approvals offer three answers: approve, approve-and-remember (the approved invocation or its parent directory becomes a persistent allow rule), deny.
- **Read-only classification** — `ToolCapabilities.read_only` marks the ~20 observing tools; plan mode hides and refuses everything else. MCP/dynamic tools default to `read_only: false` and are therefore excluded from plan mode until classified.
- **Circuit breaker** — Tools disabled after 3 consecutive failures
- **Privilege filtering** — Privileged tools hidden when `skill_trust == Community`
- **Content filtering** — Secret scanning and PII detection in tool outputs via `ContentFilter`
- **Retry declaration** — `ToolCapabilities` carries `idempotent` and `compensation`, because a tool call whose outcome is unknown (a timeout) is exactly when a caller decides whether to try again, and only the tool knows. The default is the careful one — *not* idempotent, no compensation — so a tool that says nothing is never assumed safe to repeat; reads (`file_read`, `grep`, …) declare `idempotent: true`, the file writers name the undo they support ("write the content you read back"), and the one-way ones (`shell`, `send_message`, `delegate`, `mcp__*`) say so explicitly. `ToolRegistry::uncertainty_note` quotes the declaration on the timeout path. There is no rollback machinery for side effects, and the declaration exists so nothing pretends there is.

### MCP Integration

`mcp.rs` supports Model Context Protocol servers:
- Auto-discover tools from MCP servers
- Dynamic tool registration via `register_dynamic()`
- Prefix-based cleanup when servers disconnect (`deregister_prefix()`)

## Key Types

```rust
pub struct ToolContext {
    /// Identity fields (who is calling the tool)
    pub identity: ToolIdentity,
    /// Sandbox / execution environment
    pub sandbox: ToolSandbox,
    /// Model / policy metadata
    pub model: ToolModel,
    /// Active delegation scope for delegated child agents
    pub delegation: Option<DelegationScope>,
    /// Ask queue for the `ask_user` clarification tool (None in
    /// non-interactive contexts)
    pub ask_queue: Option<Arc<AskQueue>>,
}

pub struct ToolIdentity {
    pub user_id: String,
    pub conversation_id: String,
    pub sender_id: Option<String>,
}

pub struct ToolSandbox {
    pub working_directory: PathBuf,
    pub environment: HashMap<String, String>,
    pub timeout: Duration,
    pub allowed_paths: Vec<PathBuf>,
    pub allowed_commands: Vec<String>,
    pub sandboxed: bool,
    pub memory_limit: Option<usize>,
    pub cpu_limit: Option<u64>,
    pub fd_limit: Option<u64>,
    pub process_limit: Option<u64>,
    pub workspace_root: PathBuf,
    /// Owning agent's own workspace (differs from `workspace_root` for
    /// delegated children; where reports/artifacts are written)
    pub agent_workspace: Option<PathBuf>,
    pub workspace_only: bool,
    pub sandbox_policy: Option<SandboxPolicy>,
    pub plugin_allowlist: Option<Vec<String>>,
}

pub struct ToolModel {
    pub model_name: Option<String>,
    pub provider_name: Option<String>,
    pub model_capabilities: ModelCapabilities,
    pub skill_trust: SkillTrust,
    pub tool_policy: Option<ToolPolicy>,
}
```

```rust
pub enum SkillTrust {
    Community = 0,
    Trusted = 1,
}
```

```rust
pub enum RiskLevel {
    None,
    Low,
    Medium,
    High,
    Critical,
}
```

## Implemented Features

- Unified `Tool` trait with async execution
- Tool registry with registration, lookup, and execution
- Execution context with sandboxing and resource limits
- Path traversal and command injection detection
- Workspace boundary enforcement
- Approval queue with risk-level-based filtering
- Circuit breaker for failing tools
- Skill trust-based privilege filtering
- MCP client integration with auto-discovery
- Dynamic tool registration and deregistration
- Content filtering with secret scanning and PII detection
- Command detection layer for parsing structured commands from messages
- Streaming tool execution with chunk-based output
- Model and provider-based tool gating
- Sender-based tool access control
- Pre/post-execution hook points with deny, approval-routing, output replacement, and block-with-feedback
- Claude-Code-compatible shell hooks bridge for external gating scripts
- Kernel-enforced write fences for workspace-only command tools (Seatbelt / Landlock / AppContainer)
- Read-before-edit write guard for file-mutating tools
- Output spill of oversized results to workspace files with tail-preserving previews
- Content-addressed attachment store for tool-produced images
- Human-in-the-loop clarification via the `ask_user` tool

