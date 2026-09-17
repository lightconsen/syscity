# Syscity Architecture

Syscity is a Rust-based personal AI assistant platform. It routes messages from multiple inbound channels through an agent core to LLM providers, with persistent memory, tool execution, security controls, and physical/desktop automation.

## System Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  User Interfaces                                                              │
│  CLI · TUI · Telegram · Discord · Slack · WebSocket · Webhook · Browser     │
└───────────────────────────────────┬─────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  Gateway (Control Plane)                                                      │
│  ── HTTP/WebSocket API · channel registry · agent spawning · auth · hooks    │
└───────────────────────┬───────────────────────────────────────┬─────────────┘
                        │                                       │
                        ▼                                       ▼
        ┌───────────────────────┐                   ┌───────────────────────┐
        │  Inbound Pipeline     │                   │  Outbound Pipeline    │
        │  debounce → enrich    │                   │  format → SSE →      │
        │  → route → enqueue    │                   │  dispatch → side fx   │
        └───────────┬───────────┘                   └───────────┬───────────┘
                    │                                           │
                    ▼                                           ▼
        ┌───────────────────────┐                   ┌───────────────────────┐
        │  Agent Core           │                   │  Channels / Users     │
        │  context · memory     │                   │                       │
        │  · tool calls · ACP   │                   │                       │
        └───────┬───────────────┘                   └───────────────────────┘
                │
    ┌───────────┼───────────┬───────────────┬───────────────┐
    ▼           ▼           ▼               ▼               ▼
┌───────┐  ┌───────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐
│Memory │  │ Tools │  │Providers │  │ Computer │  │   Planner    │
│Store   │  │Registry│  │ Router   │  │ Adapter  │  │  Goal DAG    │
└───────┘  └───────┘  └──────────┘  └──────────┘  └──────────────┘
```

## Core Layers

| Layer | Responsibility | Key Module |
|-------|----------------|------------|
| Interface | CLI, TUI, chat channels, webhooks | `cli`, `channels`, `tui` |
| Control Plane | HTTP/WebSocket server, lifecycle, auth | `gateway`, `security` |
| Conversation | Session management, message routing, context | `agent`, `channels`, `inbound`, `outbound` |
| Reasoning | Tool selection, planning, desktop/server automation | `tools`, `planner`, `computer`, `capabilities` |
| Memory | Conversations, semantic search, tiered storage, dreaming | `memory` |
| Providers | LLM routing, fallbacks, streaming, cost guard | `providers`, `model_router` |
| Extensions | Plugins, skills, MCP, browser automation | `plugins`, `skills`, `mcp`, `browser` |
| Operations | Cron, heartbeat, standing orders, export | `cron`, `heartbeat`, `standing_orders`, `export` |

## Data Flow

### Inbound Message

```
Channel event
    │
    ▼
InboundPipeline::receive()
    │
    ├──▶ Debounce / media download / identity resolution
    │
    ▼
ConversationResolver ──▶ agent_id + session_id
    │
    ▼
Agent::process_message()
    │
    ├──▶ MemoryManager::retrieve() ──▶ context memories
    ├──▶ ToolRegistry::available() ──▶ tool schemas
    └──▶ Provider::complete() ──▶ LLM response
                │
                ├──▶ Tool call ──▶ ToolRegistry::execute()
                │                    │
                │                    ├──▶ Security / sandbox validation
                │                    ├──▶ Approval queue (if required)
                │                    └──▶ Content filter on output
                │
                └──▶ Final text ──▶ OutboundPipeline
                                          │
                                          ▼
                                    Channel response
```

### Tool Execution

```
LLM tool call
    │
    ▼
ToolRegistry::execute()
    │
    ├──▶ Command / path / sandbox validation
    ├──▶ Approval check (human-in-the-loop)
    ├──▶ Capability scope check (os_control)
    ├──▶ Execute tool
    ├──▶ Secret / PII scan on output
    └──▶ Return result to LLM
```

### Goal Planning (Physical / OS Automation)

```
User goal
    │
    ▼
GoalPlanner::achieve()
    │
    ├──▶ GoalDecomposer ──▶ Task DAG
    ├──▶ DagScheduler ──▶ parallel execution
    │       │
    │       └──▶ TaskExecutor
    │               │
    │               ├──▶ ComputerAdapter (desktop/server)
    │               ├──▶ VerificationEngine
    │               └──▶ RollbackManager (on failure)
    │
    └──▶ Record experience to memory
```

## Key Design Decisions

1. **Trait-based abstractions** — `Channel`, `Provider`, `MemoryStore`, `Tool`, `ComputerAdapter`, `CapabilitySet` are traits, enabling pluggable implementations.
2. **`Arc<dyn ...>` for shared state** — Runtime backend selection (unified SQLite vs. tiered memory, multiple LLM providers).
3. **Feature-gated channels and tools** — Cargo features keep binaries small; optional vision, pgvector, with sqlite-vec enabled by default.
4. **Tiered memory** — Working (in-memory), ShortTerm/LongTerm (SQLite), Archival (compressed JSONL) with `TierEvaluator` promotion/demotion.
5. **CapabilitySet + ToolRegistry** — OS-specific tools are grouped by platform/environment, runtime-detected, and exported individually into `ToolRegistry`.
6. **Security-first execution** — Path/command validation, sandboxed resource limits, approval levels, RBAC, content filtering, audit logging.
7. **Planner + ComputerAdapter** — High-level goals decompose into task DAGs executed against a unified desktop/server abstraction.
8. **Runtime invariant registry** — Modules own the data invariants they uphold and register checks with `core::invariants`; `syscity invariants` runs them all against live local state. A `static-analysis.sh --full` rule enforces the declaration half for every top-level module: register checks or carry an explicit `INVARIANTS-NONE:` marker, so nothing is silently unchecked. The registry itself is young, and the two numbers are worth keeping apart: four modules register checks (`agent::session_store`, `agent::todo`, `cron::cron`, `cloud`) while 54 declare none. The declaration rule is what keeps that visible rather than the coverage being uniformly thin by accident.

## Module Documentation Map

- [`modules/acp.md`](modules/acp.md) — Agent Control Plane (subagents, sessions, execution control)
- [`modules/agent.md`](modules/agent.md) — Agent orchestration, context, turns, artifacts
- [`modules/browser.md`](modules/browser.md) — Browser automation, CDP, ARIA snapshots, browser pool
- [`modules/canvas.md`](modules/canvas.md) — A2UI component system for rich assistant UI
- [`modules/capabilities.md`](modules/capabilities.md) — Platform capability sets and OS control scopes
- [`modules/channels.md`](modules/channels.md) — Channel interfaces, resolver, thread binding
- [`modules/cli.md`](modules/cli.md) — Command-line interface
- [`modules/computer.md`](modules/computer.md) — Cross-platform desktop/server automation
- [`modules/config.md`](modules/config.md) — Configuration loading, validation, hot reload
- [`modules/core.md`](modules/core.md) — Domain models and shared types
- [`modules/cron.md`](modules/cron.md) — Scheduled task execution
- [`modules/export.md`](modules/export.md) — Conversation and memory export
- [`modules/gateway.md`](modules/gateway.md) — Gateway control plane
- [`modules/heartbeat.md`](modules/heartbeat.md) — Periodic wake/heartbeat
- [`modules/inbound.md`](modules/inbound.md) — Inbound message pipeline
- [`modules/mcp.md`](modules/mcp.md) — Model Context Protocol
- [`modules/memory.md`](modules/memory.md) — Memory and storage system
- [`modules/model_router.md`](modules/model_router.md) — LLM provider routing
- [`modules/outbound.md`](modules/outbound.md) — Outbound response pipeline
- [`modules/planner.md`](modules/planner.md) — Goal planning and task execution
- [`modules/plugins.md`](modules/plugins.md) — Plugin system
- [`modules/providers.md`](modules/providers.md) — LLM provider implementations
- [`modules/security.md`](modules/security.md) — Security layer
- [`modules/skills.md`](modules/skills.md) — Skill system
- [`modules/standing_orders.md`](modules/standing_orders.md) — Standing background agent programs
- [`modules/tools.md`](modules/tools.md) — Tool system
- [`modules/tui.md`](modules/tui.md) — Terminal UI client
- [`modules/utils.md`](modules/utils.md) — Utilities (batch, logging, pool, profiling)
- [`os.md`](os.md) — Operating-system control architecture and roadmap

## Technology Stack

- **Language**: Rust (tokio async runtime)
- **Web framework**: Axum
- **CLI**: clap
- **TUI**: ratatui + crossterm
- **Serialization**: serde + toml + json
- **Database**: SQLite (sqlx), optional Postgres (pgvector)
- **Observability**: tracing + Prometheus metrics
- **Plugins**: WASM + wapm-style registry
