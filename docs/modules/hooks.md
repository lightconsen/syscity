# Hooks Module

Claude-Code-compatible shell hooks bridge — interception, audit, and observation expressed as plain shell commands, with no plugin ABI and no recompilation.

## Design

Hooks are configured in a standalone `hooks.json` (same schema as Claude Code's `settings.json` hooks) and loaded **once at startup**. The bridge consumes the in-process `ToolHooks` API: it is a shell-based producer of `ToolHooks` plus two gateway dispatcher seams.

- **`ShellHookBridge`** (`bridge.rs`) — Owns the parsed config and exposes `tool_hooks()` (one policy + one post-execute hook), `check_user_prompt()` (the `UserPromptSubmit` gate), and `fire_stop()` (the `Stop` fan-out). Clone-cheap (`Arc`-backed).
- **`ShellHooksConfig`** / **`HookEvent`** (`config.rs`) — Parsing and validation of `hooks.json`; one hook vector per event.
- **`MatchedHook`** / `matching_hooks` (`matcher.rs`) — CC matcher semantics: a `|`-separated list of alternatives, each an exact name or a glob with `*` at the start and/or end. A missing matcher defaults to `*` (matches everything).
- **`run_command`** / decision parsers (`executor.rs`) — Run one hook command with the context JSON on stdin and a hard 10 s timeout (`HOOK_TIMEOUT`), then parse stdout into a decision.

### Events

| CC event | Syscity seam | Decisions |
|----------|--------------|-----------|
| `PreToolUse` | `ToolHooks` policy hook (before a tool executes) | `{"permission":"deny"}` hard-blocks with the reason; `"ask"` routes through the approval queue |
| `PostToolUse` | `ToolHooks` post-execute hook (after a tool finishes) | `{"decision":"block"}` confiscates the result into an error carrying the feedback; `"replace"` substitutes `additionalContext` |
| `UserPromptSubmit` | `send_to_agent` gate in the gateway dispatcher | `block` drops the message with a `chat.error` before any agent spawn |
| `Stop` | Fire-and-forget fan-out after the turn ends | Output discarded; detached so a slow hook never delays turn teardown |

Multiple matching hooks fold by rank: **deny > ask > allow** (ties keep the earlier decision) for `PreToolUse`, and **block > replace > accept** (ties take the later decision, mirroring the in-order replace chain) for `PostToolUse`.

### Contract

- **Fail-open.** A hook that crashes, times out, exits non-zero, or prints unparsable output degrades to the permissive default — a broken hook can never lock the agent out. Anomalies are `warn!`-logged.
- **Parameters are never rewritten.** CC's `updatedInput` is parsed but deliberately ignored: a hook may block a call or confiscate its result, but can never mutate the arguments the tool actually receives, so logs, execution, and UI stay consistent.
- **No hot reload.** Changes to `hooks.json` take effect on the next daemon restart.
- **Audit mirroring.** Deny/ask/block decisions are mirrored to the runtime audit log as `ToolDeny`.

### Configuration

```json
{
  "version": 1,
  "hooks": {
    "PreToolUse": [
      { "matcher": "Read|Write", "hooks": [ { "type": "command", "command": "./guard.sh" } ] }
    ],
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "./gate.sh" } ] }
    ]
  }
}
```

Resolution order for the hooks file: the explicit `hooks_file` gateway option, then a `hooks.json` sibling of the config file, then `~/.syscity/hooks.json`. Validation is fail-open — malformed entries, unknown events, unknown hook types (only `command` is supported), and version mismatches are `warn!`-logged and skipped; a missing file yields no configuration and an unparsable file yields an empty one.

Hook commands receive a JSON context on stdin: `hook_event_name`, `user_id`, and (as applicable) `tool_name`, `tool_input`, `tool_response`, `prompt`, `channel`, `agent_id`, `session_id`, `cwd`, `workspace_dir`.

### Empty Config Preservation

When no hooks are configured, `tool_hooks()` returns a **truly empty** `ToolHooks`. Per call, the bridge also reports whether any hook *matched*: matched-and-allowed becomes `ToolPolicyDecision::AllowByHook` (authoritative — the `requires_approval` fallback yields to it), while no-match becomes plain `Allow` and the fallback still applies. A hook configured for one tool therefore never disables approval for another.

## Key Types

```rust
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
}

pub struct ShellHooksConfig {
    pub pre_tool_use: Vec<MatchedHook>,
    pub post_tool_use: Vec<MatchedHook>,
    pub user_prompt_submit: Vec<MatchedHook>,
    pub stop: Vec<MatchedHook>,
}

pub struct MatchedHook {
    pub matcher: String,   // e.g. "Read|Write", defaults to "*"
    pub command: String,
}

pub struct ShellHookBridge {
    config: Arc<ShellHooksConfig>,
    audit: Option<Arc<dyn AuditLogger>>,
}

pub const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
```

## Implemented Features

- Claude-Code-compatible `hooks.json` schema (strict subset; `version: 1`)
- Four events: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`
- CC matcher semantics (`|`-separated globs, `*` prefix/suffix/contains)
- `deny` / `ask` permission gating with approval-queue routing for `ask`
- Post-execute `block` (confiscate result into error with feedback) and `replace` (`additionalContext`, alias `output`)
- 10 s per-command timeout; fail-open on crash, timeout, non-zero exit, or unparsable output
- Hook stdin context with session/cwd/workspace identity
- Audit mirroring of deny/ask/block decisions as `ToolDeny`
- Startup-only load with explicit-option / config-sibling / `~/.syscity/hooks.json` resolution
- Empty config yields a truly empty `ToolHooks`, and an unmatched call reports `Allow` (not `AllowByHook`), so the `requires_approval` fallback survives both cases
