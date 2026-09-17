<p align="center">
  <img src="syscity.png" alt="Syscity" width="120" />
</p>
<h1 align="center">Syscity - AI Agent System</h1>

<p align="center">
  <a href="https://github.com/lightconsen/syscity/actions/workflows/ci.yml">
    <img src="https://github.com/lightconsen/syscity/actions/workflows/ci.yml/badge.svg" alt="CI" />
  </a>
  <a href="https://github.com/lightconsen/syscity/blob/main/LICENSE">
    <img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License" />
  </a>
  <a href="https://img.shields.io/github/stars/lightconsen/syscity">
    <img src="https://img.shields.io/github/stars/lightconsen/syscity" alt="GitHub Stars" />
  </a>
  <a href="https://discord.gg/aaXghvzD">
    <img src="https://img.shields.io/discord/1342803221369724929?logo=discord&label=Discord" alt="Discord" />
  </a>
  <a href="https://github.com/lightconsen/syscity#requirements">
    <img src="https://img.shields.io/badge/MSRV-1.75-orange.svg" alt="MSRV" />
  </a>
</p>

Syscity is an **agent system** — a runtime that lets AI agents act on your computer. Unlike chatbots that only read and write text, Syscity agents can **control your desktop**, **execute code**, **operate your browser**, and **manage your files**.

Traditional AI lives inside a browser tab. Syscity lives inside your machine.

**One agent runtime, every device.** Run the same local agent on **macOS**, **Windows**, **Linux**, **iOS**, and **Android** — your data, memory, and tools stay on your machine. What leaves it goes where you point it: your model provider, and any chat channel or MCP server you connect.

**For developers** who want to build LLM-powered automation. **For power users** who want AI to control their desktop, not just chat.

<picture>
  <source media="(prefers-color-scheme: light)" srcset="docs/assets/demo.gif" />
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/demo-dark.gif" />
  <img src="docs/assets/demo.gif" alt="Syscity Demo — Agent generates a report and shows it in the preview panel" width="800" />
</picture>
<br/>
<p align="center"><em>Agent generates a markdown report via <code>write_report</code>, then previews it in a split-panel view.</em></p>

## Platforms

One agent runtime, every device you own. Syscity runs natively on **macOS**, **Windows**, **Linux**, **iOS**, and **Android**.

| Platform | Agent experience |
|---|---|
| **macOS** | Full desktop automation — click/type, UI trees, screenshots, AppleScript, shell, browser, files |
| **Windows** | Desktop control, shell commands, file operations, browser automation, code execution |
| **Linux** | X11/Wayland desktop control, shell, files, browser, code execution |
| **iOS** | Chat, voice input, camera, location, notifications, Shortcuts/Siri |
| **Android** | Chat, voice input, camera, location, notifications, device tools |

Your agent runs **locally**: configuration, vector memory, knowledge bases, and artifacts all stay on your device. Model inference goes to the LLM provider you configure — or stays fully on-device with Ollama. Once you connect a chat channel (Slack, WhatsApp, Telegram, Feishu, WeChat), that channel's messages arrive from and go back through its platform, as they would in any client for it.

### Mobile Apps

Take Syscity with you. The same agent runtime runs natively on **iOS** and **Android** — chat, voice input, camera, location and notifications, all talking to your local gateway. Android additionally exposes device tools (screenshots, UI automation, input, app management, pairing); on iOS that surface is smaller — device listing, screenshots and app management — because UI automation there needs WebDriverAgent, which is not implemented yet.

| iOS | Android |
|---|---|
| <picture><source media="(prefers-color-scheme: light)" srcset="docs/assets/mobile-ios-light.png" /><source media="(prefers-color-scheme: dark)" srcset="docs/assets/mobile-ios-dark.png" /><img src="docs/assets/mobile-ios-light.png" alt="Syscity on iOS" width="300" /></picture> | <picture><source media="(prefers-color-scheme: light)" srcset="docs/assets/mobile-android-light.png" /><source media="(prefers-color-scheme: dark)" srcset="docs/assets/mobile-android-dark.png" /><img src="docs/assets/mobile-android-light.png" alt="Syscity on Android" width="300" /></picture> |

## Why Syscity?

Most "AI agents" today are just chatbots with function calling — they can fetch data or send emails, but they can't *act* on your machine. Syscity is different:

**Syscity agents control your computer, not just your API keys.**

- **Your desktop is the canvas** — Click buttons, type text, read UI trees, take screenshots. Not just chat.
- **Your browser, automated** — Navigate, fill forms, capture network requests, debug console errors with sourcemaps. The agent debugs like a developer.
- **Your tools, connected** — MCP servers, shell commands, file operations, AppleScript. Bring your own ecosystem.
- **Your data, private** — Memory, knowledge bases and artifacts live on your machine, with no service of ours in the middle. Only the endpoints you configure — your model provider, the chat channels you connect — see what you send them.
- **Every platform, one agent** — macOS, Windows, Linux, iOS, Android. The same local runtime and memory, on every device you own.
- **Multiple models, one agent** — Swap between OpenAI, Anthropic, DeepSeek, GLM, Ollama, or custom endpoints. Use the right model for each task.

You don't need a new IDE, a cloud subscription, or a complex deployment. Just `curl | bash` and start.

## What is an Agent System?

An agent system bridges language models with real computing environments:

| Capability | Description |
|---|---|
| Desktop Control | Click, type, scroll, keyboard shortcuts |
| System Automation | AppleScript / shell commands / services |
| Code Execution | Run Python, JavaScript, shell scripts safely |
| Browser Automation | Navigate, click, fill forms, scrape data |
| File Management | Create, edit, move, delete, patch files |
| Web Search | Search the internet for real-time information |

Syscity provides the **action layer**, **memory layer**, and **control plane** that turn a language model into a capable software agent.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      Interaction Layer                      │
│  Web UI · Desktop App · CLI · Telegram · Discord · Slack    │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                      Control Plane (Gateway)                │
│  Auth · Rate Limiting · WebSocket · ACP Protocol · Webhooks │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                      Agent Runtime                          │
│  LLM Routing · Tool Loop · Memory · Sub-Agents (ACP) · MCP  │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                      Physical Layer                         │
│  Screenshot · Desktop Control · Accessibility · AppleScript │
│  Shell · File System · Browser · Code Execution · Web Search│
└─────────────────────────────────────────────────────────────┘
```

## Action

- 🖥️ **Desktop Control** — Click, type, scroll, and send keyboard shortcuts (macOS)
- 🍎 **AppleScript** — Control macOS applications (Mail, Finder, Calendar, etc.)
- ⌨️ **Shell Commands** — Execute bash/zsh commands in a sandboxed environment
- 🐍 **Code Execution** — Run Python, JavaScript, or shell scripts safely
- 🌐 **Browser Automation** — Navigate, click, fill forms, and scrape data
- 📁 **File Operations** — Create, edit, move, delete, and patch files

## Cognition

- 🤖 **Multi-Provider LLM** — OpenAI, Anthropic, DeepSeek, Azure, Ollama, and custom endpoints
- 🔄 **Sub-Agents (ACP)** — Spawn and delegate to sub-agents via the Agent Control Protocol
- 🧠 **Vector Memory** — Long-term semantic memory with conversation history
- 🔌 **MCP Support** — Model Context Protocol servers for external tool integration
- ⚡ **WASM Plugins** — Extend capabilities with sandboxed WebAssembly plugins

## Quick Start

### Install

```bash
# macOS / Linux
curl -sSL https://syscity.net/install.sh | bash
```

See [docs/build.md](docs/build.md) to build from source.

### Configure

```bash
# Interactive setup wizard
syscity setup
```

Config is saved to `~/.syscity/syscity.toml`.

### Start

```bash
# Start the daemon (web UI + API + WebSocket)
syscity start

# Or run in the foreground
syscity start --foreground
```

Open `http://127.0.0.1:18080` for the Web UI.

### Agent in Action

Open the Web UI, or attach the terminal client:

```bash
# Interactive terminal UI (connects to the running daemon)
syscity tui
```

Then ask the agent something like *"Take a screenshot and tell me what's on
my screen"*. The agent can:

- Capture your screen
- Read the UI tree of frontmost windows
- Click buttons or type text
- Execute AppleScript to control apps
- Run shell commands and return results

See the [Getting Started guide](docs/getting-started.md) for a full walkthrough.


## macOS Desktop Control (Best Experience)

On macOS, Syscity unlocks the full desktop automation stack:

| Tool | What it does |
|---|---|
| `macos_screenshot` | Capture full screen, window, or region |
| `macos_accessibility` | Read UI tree of any application |
| `macos_desktop_control` | Click, type, scroll, keyboard shortcuts |
| `applescript` | Control Mail, Calendar, Finder, Music, etc. |

Grant **Screen Recording** and **Accessibility** permissions in System Settings for full capability.

Desktop control also works on **Windows** and **Linux** (X11/Wayland); macOS offers the deepest integration.

## Configuration

```bash
# Set LLM provider and key
syscity config set providers.openai.api_key=sk-xxxxx
syscity config set model=gpt-4o

# Or use environment variables
export SYSCITY_API_KEY="your-api-key"
export SYSCITY_MODEL="gpt-4o"
```

## Documentation

- [Getting Started](docs/getting-started.md)
- [Build from Source](docs/build.md)
- [Architecture](docs/arch.md)
- [OS Capability Architecture](docs/os.md)
- [Protocol](docs/protocol.md)
- [Slash Commands](docs/command.md)
- [Full documentation index](docs/README.md)

## License

Apache-2.0

## Contributing

PRs are welcome! Check out the [issues](https://github.com/lightconsen/syscity/issues) or join the discussion on [Discord](https://discord.gg/aaXghvzD).
