# Syscity Documentation

Documentation for **Syscity**, an OS for Physical AI — a runtime that lets AI
agents perceive and act on your computer.

## Start here

| Doc | What it covers |
|-----|----------------|
| [Getting Started](getting-started.md) | Install, configure, and run your first agent |
| [Build from Source](build.md) | Prerequisites, feature flags, desktop app build |
| [Release Process](release.md) | Version bump, changelog-driven notes, tagging, updater signing |
| [Architecture](arch.md) | System overview, control plane, pipelines |
| [Slash Commands](command.md) | `/` commands available in chat, TUI, and channels |
| [Goal-Based Execution](goal.md) | `/goal` autonomous execution with stop conditions |
| [Channels Setup](channels.md) | Supported channels, credentials, and webhook configuration |

## Core concepts

| Doc | What it covers |
|-----|----------------|
| [OS Capability Architecture](os.md) | Physical/desktop capability model |
| [Protocol](protocol.md) | WebSocket RPC + ACP protocol surface |
| [Security Configuration](security-config.md) | Auth, rate limiting, CORS/CSP, device pairing |
| [Secret Storage](secret-storage.md) | Sensitive-info inventory + keyring/file storage design |
| [Self-Upgrade](self-upgrade.md) | In-place upgrade flow |
| [State Snapshot](state-snapshot.md) | State capture / restore |
| [Tools Testing](tools_testing.md) | How tools are tested |

## Benchmarks & evals

| Doc | What it covers |
|-----|----------------|
| [Benchmark Results](benchmark.md) | Current BFCL / GAIA scores first, then the plan and method for public benchmarks |
| [Computer Use Benchmark](computer-use-benchmark.md) | Plan for scoring syscity on OSWorld-Verified and Cua Bench (Chinese) |
| [OSWorld on a KVM Server](osworld-kvm-server.md) | Runbook: provision a KVM host, verify the VM, run the OSWorld × syscity harness (Chinese) |
| [Eval Harness](harness.md) | The eval loop: suites, trials, judges, badcases, the ledger |
| [Eval Status](eval-status.md) | Methodology and what is landed |

## Module reference

Per-module design docs live in [`modules/`](modules/). Each maps to a
directory under `src/`.

### Control plane & runtime
- [gateway](modules/gateway.md) — control-plane entry: HTTP/WS API, agent spawning, hot-reload
- [agent](modules/agent.md) — agent core: context, tool loop, prompt building
- [inbound](modules/inbound.md) — inbound message pipeline (debounce → route)
- [outbound](modules/outbound.md) — outbound response pipeline (format → dispatch → side effects)
- [core](modules/core.md) — Id / Engine / EventBus primitives
- [config](modules/config.md) — configuration + hot-reload + secret resolution
- [security](modules/security.md) — auth, allowlist, rate limiting

### Cognition
- [providers](modules/providers.md) — LLM provider integrations
- [model_router](modules/model_router.md) — model selection / routing
- [memory](modules/memory.md) — long-term + semantic memory
- [goal](modules/goal.md) — goal-based execution with structured stop conditions
- [planner](modules/planner.md) — goal decomposition + DAG execution
- [skills](modules/skills.md) — skill storage + triggering

### Tools & extension
- [tools](modules/tools.md) — tool registry, RBAC, approval, circuit breakers
- [mcp](modules/mcp.md) — Model Context Protocol (stdio/sse/http)
- [MCP Servers Guide](mcp-servers.md) — how to add and connect MCP servers
- [acp](modules/acp.md) — Agent Control Protocol / sub-agents
- [plugins](modules/plugins.md) — WASM plugin sandbox + hot-reload
- [hooks](modules/hooks.md) — Claude-Code-compatible shell hooks (hooks.json)
- [canvas](modules/canvas.md) — A2UI dynamic UI components

### Physical / execution layer
- [computer](modules/computer.md) — cross-platform desktop control
- [browser](modules/browser.md) — browser automation
- [capabilities](modules/capabilities.md) — capability profiles + platform constraints

### Channels & I/O
- [channels](modules/channels.md) — multi-channel transport (Telegram/Discord/Slack/…)
- [cli](modules/cli.md) — command-line interface
- [tui](modules/tui.md) — terminal UI
- [export](modules/export.md) — transcript export (md/json/jsonl)

### Background services
- [cron](modules/cron.md) — scheduled tasks
- [heartbeat](modules/heartbeat.md) — periodic wake (interval-based)
- [standing_orders](modules/standing_orders.md) — cron-like background agents
- [observe](modules/observe.md) — per-turn observability records + `syscity observe` CLI

### Infrastructure
- [adapters](modules/adapters.md) — storage backends (InMem/File/Sqlite)
- [utils](modules/utils.md) — batch/pool/profiling/logging helpers

## Contributing

See [CONTRIBUTING.md](../CONTRIBUTING.md), [CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md),
and [SECURITY.md](../SECURITY.md) in the repository root.
