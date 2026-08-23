<div align="center">

# Grodex

**An open-source AI coding agent built in Rust — auditable, recoverable, and sandboxed.**

[![Crates.io](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Build](https://img.shields.io/badge/build-passing-green.svg)]()

[English](README.md) · [中文](README.zh.md)

</div>

---

Grodex is a CLI tool and ACP server that reads, edits, and runs code in your project. It features a crash-recoverable agent loop, kernel-enforced sandboxing, and a provider-agnostic architecture that works with OpenAI, Anthropic, DeepSeek, and more.

## Why Grodex?

| | Grodex | Typical AI Coding Tools |
|---|---|---|
| **Crash recovery** | Append-only rollout journal — resume any session from exact state | Lose context on crash |
| **Sandbox** | macOS Seatbelt kernel-enforced, Linux Landlock ready | Optional or none |
| **Auditability** | 17 runtime invariants, every action journaled | Black box |
| **Provider lock-in** | 3 wire protocols, multi-provider failover | Single vendor |
| **Sub-agents** | Delegation envelopes with authority ceilings | Flat or absent |
| **Transparency** | Open-source, 21 crates, 550+ tests | Closed-source |

## Features

- **Agent Loop** — Session Supervisor → Turn Coordinator → Sampling Step, with parallel tool dispatch and model-order commit
- **Crash Recovery** — Every state change recorded in an append-only JSONL journal; `resume` rebuilds from exact crash point
- **5 Built-in Tools** — `read_file`, `write_file`, `edit_file`, `exec`, `apply_patch` with permission gates and sandbox enforcement
- **Sub-agent Delegation** — Spawn child agents with frozen authority boundaries, concurrency caps, and live TUI progress cards
- **Context Management** — Auto window detection, percentage-based compaction, oversized result offloading
- **Multi-Provider** — OpenAI Responses API, Chat Completions (DeepSeek/Qwen thinking mode), Anthropic Messages API
- **Credential Broker** — Master tokens never exposed to agents; single-use leases with anti-replay
- **Memory** — SQLite + FTS5 with hybrid retrieval (BM25 + vector), injected into system prompt
- **MCP Support** — Spawn and communicate with MCP servers via stdio JSON-RPC
- **TUI** — Vim-style modal interface with thinking panel, sub-agent cards, and approval workflow

## Quick Start

### Prerequisites

- **Rust 1.85+** (edition 2024)
- An API key for your model provider

### Install & Run

```bash
# Clone and build
git clone https://github.com/yikaikkk/grodex.git
cd grodex
cargo build --release

# Start an interactive session
./target/release/grodex run

# Or via cargo
cargo run -- run
```

### Configuration

Create `~/.grodex/config.toml`:

```toml
provider = "openai"
model = "gpt-5"
wire_protocol = "responses"   # "responses" | "chat" | "messages"

# Sandbox profile
sandbox_profile = "workspace"  # "workspace" | "readonly" | "restricted" | "full"

# Permission rules
# [rules]
# read_file = "allow"
# write_file = "ask"
# exec = "ask"
```

> See [config.example.toml](config.example.toml) for multi-provider failover, context window tuning, and all options.

### Environment Variables

| Variable | Description |
|---|---|
| `OPENAI_API_KEY` | OpenAI API key |
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `GRODEX_PROVIDER` | Override provider (`openai` / `anthropic` / `deepseek`) |
| `GRODEX_MODEL` | Override model name |
| `GRODEX_WIRE_PROTOCOL` | Override wire protocol |
| `GRODEX_API_ENDPOINT` | Override API endpoint URL |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        grodex-cli / grodex-tui                   │
│                     (Interactive TUI · ACP Client)               │
├─────────────────────────────────────────────────────────────────┤
│                      grodex-protocol (ACP)                       │
│              EventEnvelope · SessionSnapshot · stdio             │
├─────────────────────────────────────────────────────────────────┤
│                        grodex-loop                               │
│   SessionSupervisor → TurnCoordinator → SamplingStep → Reducer  │
│                     RolloutWriter (JSONL journal)                │
├──────────────┬──────────────┬───────────────┬───────────────────┤
│ grodex-      │ grodex-      │ grodex-       │ grodex-           │
│ sampler      │ provider     │ capability    │ subagent          │
│ (HTTP +      │ (Canonical   │ (Prepared     │ (Delegation       │
│  streaming)  │  IR)         │  Call fence)  │  envelopes)       │
├──────────────┴──────────────┴───────────────┴───────────────────┤
│  grodex-tools · grodex-permission · grodex-sandbox · grodex-auth │
├─────────────────────────────────────────────────────────────────┤
│  grodex-prompt · grodex-memory · grodex-config · grodex-skills  │
│                      grodex-mcp                                 │
└─────────────────────────────────────────────────────────────────┘
```

<details>
<summary><strong>21 crates</strong> — click to expand project layout</summary>

```
grodex/
├── crates/
│   ├── grodex-core/            # Shared types: ContextItem, IDs, PolicyDecision
│   ├── grodex-loop/            # Agent loop: Supervisor, TurnCoordinator, Reducer
│   ├── grodex-provider/        # Canonical request/event model, wire descriptors
│   ├── grodex-sampler/         # HTTP client, streaming decoders (3 protocols)
│   ├── grodex-capability/      # Capability descriptors, PreparedCapabilityCall
│   ├── grodex-permission/      # Policy engine, approval broker, leases
│   ├── grodex-sandbox/         # Seatbelt enforcement, path validation
│   ├── grodex-tools/           # Built-in tools: read, write, edit, exec, patch
│   ├── grodex-subagent/        # Sub-agent tree, delegation envelopes
│   ├── grodex-auth/            # Credential broker, secret store
│   ├── grodex-config/          # TOML config, layered merge
│   ├── grodex-protocol/        # ACP types, EventEnvelope, stdio transport
│   ├── grodex-skills/          # Skill catalog, filesystem discovery
│   ├── grodex-mcp/             # MCP client, JSON-RPC process management
│   ├── grodex-memory/          # SQLite + FTS5 memory store
│   ├── grodex-prompt/          # Prompt builder, instruction assembly
│   ├── grodex-cli/             # CLI entry: run, serve, resume, replay
│   └── grodex-tui/             # Terminal UI (ratatui + crossterm)
├── docs/                       # Design documents (13 files)
└── config.example.toml
```

</details>

## Design Invariants

Grodex enforces **17 runtime invariants** — not just documented, but verified in tests:

| # | Invariant |
|---|---|
| 1 | Session state transitions serialised through Supervisor |
| 2 | At most one Turn admitted at a time |
| 3 | Model can only call tools exposed by current StepSnapshot |
| 4 | Tool calls bound to capability revision at dispatch |
| 5 | No side effect before permission gate clears |
| 6 | Tool results committed in model-emission order |
| 7 | Result durable before next sampling step |
| 8 | Cancellation waits for cleanup before new Turn |
| 9 | Compaction leaves no dangling tool calls |
| 10 | Memory snapshot stable within a Turn |
| 11 | Background task completion ≠ main agent has read it |
| 12 | Child agent authority ≤ parent ceiling |
| 13 | `rollout.jsonl` is the single source of truth |
| 14 | Late events rejected via generation comparison |
| 15 | Tool / Skill / MCP stable within a Turn |
| 16 | Revocation only tightens (epoch monotonic) |
| 17 | AppOnly actions recorded in rollout |

## TUI

Vim-style modal interface with thinking panel, sub-agent cards, and integrated approval workflow:

| Key | Action |
|---|---|
| `i` / `:` | Enter prompt / command mode |
| `Enter` | Send prompt |
| `Ctrl`+`O` | Toggle thinking (CoT) panel |
| `Ctrl`+`E` | Toggle sub-agent execution log |
| `↑`/`↓` + `Enter` | Select and confirm approval |
| `Ctrl`+`C` | Cancel streaming (double-press to quit) |

## Testing

```bash
# Run all tests (550+)
cargo test --workspace

# Run a specific crate
cargo test -p grodex-loop
```

Key coverage: crash recovery (6 scenarios), generation fence, concurrent scheduling, golden wire-event replay (3 protocols), macOS Seatbelt kernel deny-path, delegation authority ceiling, credential lease anti-replay.

## Documentation

Deep-dive design documents live in [`docs/`](docs/):

| Document | Description |
|---|---|
| [Agent Loop](docs/09-agent-loop-v2-design.md) | Supervisor → TurnCoordinator → SamplingStep |
| [Context Management](docs/11-context-management-v2-design.md) | Rollout journal, compaction, projection |
| [Provider Adapter](docs/14-provider-model-adapter-v2-design.md) | Canonical events, wire protocol decoding |
| [Built-in Tools](docs/15-built-in-tools-v2-design.md) | Tool specifications and contracts |
| [Permission System](docs/16-permission-approval-execution-v2-design.md) | Policy engine, approval, leases |
| [ACP Protocol](docs/17-frontend-acp-protocol-v2-design.md) | Event envelopes, streaming, resync |

## Contributing

Contributions are welcome! Please:

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Ensure tests pass (`cargo test --workspace`)
4. Commit your changes (`git commit -m 'feat: add amazing feature'`)
5. Open a Pull Request

## License

Licensed under either of:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.

---

<div align="center">

**Built with Rust 🦀 — designed for safety, transparency, and recoverability.**

</div>
