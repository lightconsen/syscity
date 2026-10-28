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
    <img src="https://img.shields.io/badge/MSRV-1.97-orange.svg" alt="MSRV" />
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

## Install

```bash
# macOS / Linux
curl -sSL https://syscity.net/install.sh | bash
```

`syscity setup` picks a model provider; `syscity start` serves the Web UI at
`http://127.0.0.1:18080`. To build from source, see [docs/build.md](docs/build.md).

## Benchmarks

Scores come from each benchmark's **official evaluator**, never a homegrown
scorer. Model: **`deepseek-v4-flash`** throughout.

| Benchmark | What it measures | Score |
|---|---|---|
| [BFCL](https://gorilla.cs.berkeley.edu/leaderboard.html) `bfcl-eval 2026.3.23` | the model plus the OpenAI-compatible wire (`raw` mode) | **80.12%** (673/840) over two runs (78.81% then 80.12%, a difference inside the noise floor) |
| [GAIA](https://huggingface.co/datasets/gaia-benchmark/GAIA) validation Level 1 | syscity's own agent loop, end to end | **78.0%** (124/159 trials), from a 70.4% baseline before the retrieval and answer-format work |

Finer-grained tables, the method behind each number, the measured noise floor,
reproduction steps and the failure breakdown are in
**[docs/benchmark.md](docs/benchmark.md)**. τ²-bench is next.

## Overview

Platforms and mobile apps, why Syscity exists, the architecture diagram, the
action/cognition tool lists, the full quick start, macOS desktop control and the
configuration reference: [docs/overview.md](docs/overview.md).

## Documentation

- [Getting Started](docs/getting-started.md)
- [Overview](docs/overview.md)
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
