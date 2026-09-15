# Getting Started

This guide takes you from a fresh install to your first physical-AI agent in a
few minutes.

> **Prerequisite:** a Syscity binary. Either install a release or
> [build from source](build.md). Check it works:
> ```bash
> syscity --version
> ```

## 1. Run the setup wizard

```bash
syscity setup
```

The interactive wizard walks you through choosing an LLM provider, entering an
API key, and picking a default model. It writes everything to
`~/.syscity/syscity.toml`.

Prefer to configure by hand? Skip the wizard and set values directly:

```bash
syscity config set providers.openai.api_key=sk-xxxxx
syscity config set model=gpt-4o
```

Or via environment variables (handy for CI / containers):

```bash
export SYSCITY_API_KEY="your-api-key"
export SYSCITY_MODEL="gpt-4o"
```

Validate the configuration at any time:

```bash
syscity config            # show current config
syscity doctor run        # run diagnostic checks
```

## 2. Start the daemon

```bash
# Background daemon: web UI + HTTP API + WebSocket on port 18080
syscity start

# Or stay in the foreground (logs to your terminal)
syscity start --foreground
```

Check it came up:

```bash
syscity status
syscity logs -f          # tail the logs
```

The Web UI is now at **http://127.0.0.1:18080**.

## 3. Talk to your agent

Two interactive options:

**Web UI** — open http://127.0.0.1:18080 in a browser.

**Terminal client** — attach the TUI to the running daemon:

```bash
syscity tui                 # fresh conversation
syscity tui --continue      # resume the most recent session
syscity tui --resume        # list sessions and pick one with /resume <n>
```

The TUI runs inline: the conversation goes into your terminal's own scrollback,
so normal scrolling, text selection and post-exit history all work.

Try a physical-AI prompt:

> Take a screenshot and tell me what's on my screen.

On macOS the agent can also read the accessibility tree, click and type, run
AppleScript, and execute shell commands. Grant **Screen Recording** and
**Accessibility** permissions in *System Settings → Privacy & Security* for the
full experience.

## 4. Connect a messaging channel (optional)

Syscity can run as a bot on Telegram, Discord, Slack, and more:

```bash
# Example: add a Telegram bot
syscity channel add telegram --token <BOT_TOKEN>

# List configured channels
syscity channel list
```

See [channels](modules/channels.md) for per-platform setup.

## 5. Useful day-to-day commands

| Command | Purpose |
|---------|---------|
| `syscity status` | Is the daemon running? |
| `syscity logs -f` | Tail daemon logs |
| `syscity reload` | Reload config + plugins without restarting |
| `syscity stop` | Stop the daemon |
| `syscity session` | Inspect / manage sessions |
| `syscity memory` | Search and manage vector memory |
| `syscity skill` | Manage skills |
| `syscity provider` | List / switch LLM providers |
| `syscity capabilities` | Show available OS capability sets |
| `syscity export` | Export conversations / memories |

Run `syscity --help` or `syscity <command> --help` for the full list.

## Where to go next

- [Architecture](arch.md) — how messages flow through the system
- [Slash Commands](command.md) — `/` commands inside chat / TUI
- [OS Capability Architecture](os.md) — the physical / desktop layer
- [Full documentation index](README.md)
