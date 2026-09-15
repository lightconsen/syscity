# Syscity Slash Commands

`/` commands are available in Syscity's chat UI, TUI, and any channel that supports slash-command parsing (Telegram, Discord, Slack, WebChat, etc.).

The command system has two layers:

- **Gateway command catalog** (`src/gateway/commands/mod.rs`) — canonical command definitions exposed via the WebSocket RPC method `commands.list` and executed via `commands.execute`, with handlers split by domain into `admin.rs`, `agents.rs`, `model.rs`, `session.rs`, and `tools.rs`.
- **TUI slash commands** (`src/tui/commands.rs`) — client-side parser that handles a small set of local commands and forwards everything else to the gateway.

- **Local** commands run client-side without a backend round-trip.
- **Remote** commands are sent to the gateway via WebSocket RPC.
- **Admin** commands require `SCOPE_ADMIN`.

---

## Legend

| Badge | Meaning |
|---|---|
| `local` | Executed client-side |
| `admin` | Requires admin scope |
| `essential` | Core commands always shown in completions |
| `standard` | Common commands shown by default |
| `power` | Advanced/admin commands hidden unless filtering explicitly |

---

## Session Commands

| Command | Args | Description | Local | Admin |
|---|---|---|---|---|
| `/new` | `[model]` | Start a new session. In channels this is handled locally; the TUI creates a session via `sessions.create` and subscribes to it. | yes | no |
| `/reset` | `[soft\|hard]` | Reset the current session. Terminates the session, recreates it, and deletes persisted history. | no | no |
| `/stop` | — | Abort the current run by cancelling the active ACP session. | no | no |
| `/compact` | `[instructions]` | Compact the session context via `Agent::compact_context()` and flush the transcript to disk as HTML. | no | no |
| `/export-session` | `[path]` | Export the current session transcript as HTML via `TranscriptStore::export()`. | no | no |
| `/clear` | — | Clear chat history from the local message buffer (client-side only). | yes | no |
| `/session` | `idle\|max-age <duration\|off>` | Read or set session timeout values (`session.idle`, `session.max-age`). | no | no |

### Examples

```text
/new gpt-4
/reset hard
/stop
/compact summarize the last 50 messages
/export-session
/session idle 30m
/session max-age off
```

---

## Model / Directive Commands

| Command | Args | Description | Local | Admin |
|---|---|---|---|---|
| `/model` | `[name\|#\|status]` | Show the current default model and provider, or set a runtime model override. | no | no |
| `/think` | `<level>` | Set the thinking level (`off`, `minimal`, `low`, `medium`, `high`) stored as `think.level`. | no | no |
| `/verbose` | `on\|off\|full` | Toggle verbose output mode stored as `verbose.mode`. | no | no |
| `/trace` | `on\|off` | Toggle plugin trace via `PluginManager::set_trace_enabled()` and store as `trace.enabled`. | no | no |
| `/fast` | `[on\|off\|status]` | Enable or disable fast mode, which swaps `config.model` to the resolved `fast` alias and restores it on disable. | no | no |
| `/reasoning` | `[on\|off\|stream]` | Set reasoning visibility stored as `reasoning.visibility`. | no | no |
| `/queue` | `<mode>` | Set queue behavior (`steer`, `interrupt`, `followup`) stored as `queue.mode`. | no | no |

### Examples

```text
/model status
/model claude-sonnet-4-6
/think high
/verbose full
/fast on
/reasoning stream
/queue steer
```

---

## Status / Query Commands

| Command | Args | Description | Local | Admin |
|---|---|---|---|---|
| `/help` | `[page] [--tier=essential|standard|power]` | Show paginated help summary grouped by category. | no | no |
| `/commands` | — | Alias for `/help`. | no | no |
| `/status` | — | Show gateway runtime status: active agents and sessions. | no | no |
| `/tools` | `[compact\|verbose]` | Show available tools registered in `ToolRegistry`. | no | no |
| `/whoami` | — | Show the authenticated user ID and granted scopes. | no | no |
| `/usage` | `[off\|tokens\|full\|cost]` | Show usage statistics from `runtime_settings`. | no | no |
| `/context` | `[list\|detail\|json]` | Show context assembly info for the active session. | no | no |

### Examples

```text
/help
/help 2 --tier=power
/status
/tools verbose
/whoami
/usage tokens
/context detail
```

---

## Subagents / ACP Commands

| Command | Args | Description | Local | Admin |
|---|---|---|---|---|
| `/subagents` | `list\|kill\|log\|info\|send\|steer\|spawn` | Manage sub-agents. `list` shows ACP status for the active session. | no | no |
| `/acp` | `spawn\|cancel\|steer\|close\|sessions\|status\|...` | Manage ACP sessions. `status` and `cancel`/`close` are implemented. | no | no |
| `/kill` | `<id\|#\|all>` | Abort sub-agent runs. Cancels the active session or shuts down a specific subagent. | no | no |
| `/steer` | `<id> <message>` | Send a steering message to a sub-agent via `acp.send_message()`. | no | no |
| `/tell` | `<id> <message>` | Alias for `/steer`. | no | no |
| `/focus` | `<target>` | Bind the current thread to an agent target via `AgentRouter::bind_session()`. | no | no |
| `/unfocus` | — | Remove thread binding via `AgentRouter::unbind_session()`. | no | no |
| `/goal` | `<description> [--max-rounds N] [--fresh]` | Run an autonomous goal loop with structured stop conditions. Subcommands: `cancel`, `list`, `resume`. | no | no |

### Goal Commands

`/goal` starts a background `GoalRunner` that iterates "agent acts → check conditions → feedback" until all LLM-generated stop conditions pass or a guardrail trips. Progress streams back as `goal.progress` events. See [Goal-Based Execution](goal.md) for the full contract.

| Subcommand | Description |
|---|---|
| `/goal <description> [--max-rounds N] [--fresh]` | Start a new goal. `--fresh` enables the fresh-context (Ralph) loop: each round is a seedless sub-agent and only a validated ```handoff JSON block (≤16K) carries state between rounds. |
| `/goal cancel <goal_id>` | Cancel a running goal; also discards the checkpoint of a suspended goal. |
| `/goal list` | List running goals plus suspended goals (round, max rounds, blocked reason). |
| `/goal resume <goal_id>` | Re-arm a suspended goal from its persisted checkpoint. |

Goals are checkpointed per round to `~/.syscity/goals/<goal_id>.json`. After a gateway restart, persisted goals are **suspended, not auto-resumed** — use `/goal list` to see them and `/goal resume <id>` to continue. Policy stops (loop detected, max rounds, invalid handoff, fatal config error) keep their checkpoint with a structured `blocked_reason`; completion, cancellation, and agent errors delete it.

### Examples

```text
/subagents list
/acp status
/acp cancel
/kill all
/steer subagent-1 stop and summarize
/focus my-agent
/unfocus
/goal write tests for src/module.rs --max-rounds 3
/goal refactor the auth module --fresh
/goal list
/goal resume goal_7f3a...
/goal cancel goal_7f3a...
```

---

## Skills / Approval Commands

| Command | Args | Description | Local | Admin |
|---|---|---|---|---|
| `/skill` | `<name> [input]` | List all skills or show details for a named skill. | no | no |
| `/allowlist` | `[list\|add\|remove] ...` | Manage command gate user levels (`chat`, `user`, `admin`). | no | no |
| `/approve` | `<id> <decision>` | List pending approvals or resolve one via `ApprovalQueue::resolve()`. | no | no |
| `/btw` | `<question>` | Side question without changing context; calls `ModelRouter::complete_auto()` directly. | no | no |

### Examples

```text
/skill summarize
/skill summarize my notes
/allowlist list
/allowlist add alice admin
/approve abc123 approve
/btw what is the weather?
```

---

## Admin Commands (Owner-Only)

| Command | Args | Description | Local | Admin |
|---|---|---|---|---|
| `/config` | `show\|get\|set\|unset` | Read or write runtime config values in `runtime_settings`. | no | **yes** |
| `/plugins` | `list\|enable\|disable` | List plugins or toggle plugin enabled state. | no | **yes** |
| `/mcp` | `show\|disconnect` | List connected MCP servers or disconnect a server. | no | **yes** |
| `/debug` | `show\|set\|unset\|reset` | Read, write, or clear runtime debug overrides. | no | **yes** |
| `/restart` | — | Restart the gateway by scheduling `std::process::exit(0)` after 1 second. | no | **yes** |
| `/bash` | `<command>` | Run a host shell command via `sh -c` and return stdout, stderr, and exit code. | no | **yes** |

### Examples

```text
/config show
/config get model.override
/config set model.override claude-opus-4-6
/plugins list
/plugins enable my-plugin
/mcp show
/mcp disconnect my-server
/debug show
/debug reset
/restart
/bash ls -la
```

---

## TUI-Only Local Commands

Handled directly by `src/tui/commands.rs`; anything not listed here is forwarded
to the gateway as a `commands.execute` request. Output is printed into the
terminal's scrollback — the TUI runs inline and has no popups.

| Command | Args | Description |
|---|---|---|
| `/new` | `[agent]` | Create a session, optionally bound to an agent. |
| `/resume` | `[n\|id]` | List sessions, or switch to one (also `--continue` / `--resume` at launch). |
| `/sessions` | — | List sessions. |
| `/rename` | `<name>` | Rename the current session. |
| `/pin` | — | Pin or unpin the current session. |
| `/agents` | — | List agents. |
| `/agent` | `<id>` | Start a session bound to an agent. |
| `/clear` | — | Clear the conversation context (`sessions.reset`); the terminal's own scrollback is untouched. |
| `/config` | `[set <path> <value>]` | Show the effective configuration, or change one value. |
| `/status` | — | Query gateway presence via `system.presence`. |
| `/tools` | — | List the gateway's command catalog via `commands.list`. |
| `/model` | `<id>` | Set the default model via `models.set_default`. |
| `/answer` | `<text>` | Answer a pending `ask_user` question. |
| `/help` | — | Keybindings and the command catalog. |
| `/quit` / `/exit` | — | Exit the TUI. |
---

## Inline Shortcuts

Removed. Embedding a command inside a message silently dropped a word from what the user typed, so commands are now only run when they are the whole message.

## Command Metadata

Each command is represented by a `CommandDef`:

```rust
pub struct CommandDef {
    pub key: String,
    pub name: String,
    pub description: String,
    pub args: Option<String>,
    pub category: CommandCategory,
    pub tier: CommandTier,
    pub local: bool,
    pub requires_admin: bool,
    pub aliases: Vec<String>,
    pub scope: CommandScope,
    pub provider_hint: Option<CommandProviderHint>,
}
```

| Field | Meaning |
|---|---|
| `key` | Canonical command key used for dispatch |
| `name` | Display name |
| `args` | Argument hint shown in help |
| `category` | `Session`, `Model`, `Status`, `Agents`, `Tools`, `Admin` |
| `tier` | `Essential`, `Standard`, `Power` |
| `local` | Runs client-side only |
| `requires_admin` | Requires `SCOPE_ADMIN` |
| `aliases` | Alternative names |
| `scope` | `Global`, `DirectMessage`, `Channel` |
| `provider_hint` | Optional provider/model override |

---

## Command Tiers

| Tier | Description |
|---|---|
| **essential** | Core commands always shown in the palette |
| **standard** | Common commands shown by default |
| **power** | Advanced/admin commands hidden unless filtering explicitly |

Tier is enforced by `CommandGate::user_level` in addition to the admin scope check.

---

## RPC Methods

The gateway command system is backed by two WebSocket RPC methods:

- **`commands.list`** — returns the full command catalog with metadata
- **`commands.execute`** — dispatches a command and returns the result

Both require `SCOPE_READ` (list) and `SCOPE_WRITE` (execute) respectively. Admin commands additionally require `SCOPE_ADMIN`.

---

## Provider/Model Hints

`CommandProviderResolver` (in `src/gateway/command_provider.rs`) maps commands to advisory provider/model hints:

| Condition | Hint | Reason |
|---|---|---|
| Explicit `provider_hint` on `CommandDef` | configured hint | highest priority |
| `UserLevel::Chat` | `fast` | low-trust users steered to cheap model |
| `CommandCategory::Admin` or `CommandTier::Power` | `power` | admin/power commands need capable model |
| Essential `Session`/`Status` commands | `fast` | cheap to answer |
| Everything else | default | no hint |

---

## Implementation Notes

| Command | Key Implementation Detail |
|---|---|
| `/new` | In the gateway catalog it is marked `local` (channel-only); the TUI handles `/new` locally by calling `sessions.create` + `sessions.subscribe`. |
| `/clear` | Client-side only; removes messages from the local buffer. |
| `/status` | Gateway handler returns active agent/session counts from `GatewayState`. |
| `/tools` | Calls `ToolRegistry::list()` and optionally shows each tool name. |
| `/whoami` | Returns the authenticated `user_id` and granted scopes from the WebSocket connection. |
| `/think` | Stores `think.level` in `runtime_settings`. The actual budget injection happens downstream before completion. |
| `/trace` | Calls `PluginManager::set_trace_enabled()` and stores `trace.enabled` in `runtime_settings`. |
| `/fast` | Swaps `config.model` to the resolved `fast` alias on `/fast on`, and restores the original model on `/fast off`. |
| `/queue` | Stores `queue.mode` in `runtime_settings`; `interrupt` mode can be consumed by `send_to_agent()` to cancel the current run. |
| `/context` | Resolves the active agent via `AgentRouter::resolve_by_session()`, then calls `Agent::context_info()` to introspect message count, tokens, and tool iterations. |
| `/compact` | Resolves the active agent, calls `Agent::compact_context()`, then flushes the transcript to disk via `TranscriptStore::export(HTML)`. |
| `/btw` | Bypasses session state by calling `ModelRouter::complete_auto()` directly for a one-shot Q&A. |
| `/restart` | Spawns a background task that sleeps 1 second then calls `std::process::exit(0)`. |
| `/bash` | Runs `sh -c <args>` and returns stdout/stderr/exit code. |
| `/approve` | Lists pending approvals or resolves one via `ApprovalQueue::resolve()`. |
| `/allowlist` | Reads/writes user levels via `CommandGate`. |
| `/goal` | Parses the description into a `GoalPlan` via an LLM call, then spawns a background `GoalRunner` (`src/goal/runner.rs`); `cancel`/`list`/`resume` are handled in `src/gateway/commands/agents.rs`, resume re-arms from checkpoint via `src/gateway/goal_spawn.rs`. |

---

## Module Relationships

```
src/gateway/commands/mod.rs     # canonical catalog and commands.list/execute dispatcher
src/gateway/commands/session.rs # session & status handlers
src/gateway/commands/model.rs   # model tuning handlers
src/gateway/commands/agents.rs  # subagent/ACP/goal handlers
src/gateway/commands/tools.rs   # tools/skill/approval handlers
src/gateway/commands/admin.rs   # admin handlers
src/gateway/command_provider.rs # provider/model hint resolution
src/tui/commands.rs             # TUI client-side slash command parser
src/channels/command_gate.rs    # channel-side command gating
src/tools/command_gate.rs       # user level store and enforcement
```
