<div align="center">

# Grodex

**An open-source AI coding agent built in Rust — auditable, recoverable, sandboxed, fully observable.**

![rust-1.85+](https://img.shields.io/badge/rust-1.85%2B-orange)
![license](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)
![build](https://img.shields.io/badge/build-passing-green)

English · [中文](README.zh.md)

</div>

---

Grodex is a CLI tool and ACP server that reads, writes, and runs code in your projects. It features a crash-recoverable Agent Loop, kernel-enforced sandboxing, end-to-end SQLite observability, and a provider-agnostic architecture supporting OpenAI, Anthropic, DeepSeek and more.

## Why Grodex?

| | Grodex | Typical AI coding tools |
|---|---|---|
| **Crash recovery** | Append-only Rollout Journal — resume any session from the exact crash point | Context lost on crash |
| **Observability** | SQLite telemetry projection: which turn was slow, which tool hung, how long approvals waited, cache hit rate — one command away | Black box or plain text logs |
| **Sandbox** | macOS Seatbelt kernel enforcement + exec resource limits (memory/CPU/processes) + process-group kill | Optional or none |
| **Auditability** | 17 runtime invariants, every action journaled; credential env vars auto-stripped | Black box |
| **Vendor lock-in** | 3 wire protocols, multi-candidate failover (credential + endpoint switch per candidate) | Single vendor |
| **Sub-agents** | Unified agent tree: delegate, message, wait, interrupt, permission ceilings | Flat or none |
| **Transparency** | Open source, 22 crates, 919 tests | Closed source |

## Features

- **Agent Loop** — Session Supervisor → Turn Coordinator → Sampling Step; parallel tool dispatch with model-order commit; bounded repair sampling (never fires on plain Q&A)
- **Crash recovery** — every state change appended to a gap-free JSONL journal; `resume` rebuilds from the exact crash point, healing interrupted tool calls
- **Observability** — every lifecycle event lands in SQLite (WAL): model attempts (TTFT / cache hit rate / retries), tool executions (approval wait vs execution time), security decisions, sub-agent runs; automatic re-projection after crashes
- **12 built-in tools** — `read_file`, `write_file`, `edit_file`, `exec`, `apply_patch`, `web_fetch`, `grep`, `glob`, `load_skill`, `read_artifact`, `process_io`, `delegate_task` — all with real descriptions, permission gates, and sandbox enforcement
- **Approval workflow** — Allow / **Always allow** (session-persistent grant) / Deny / **Narrow** (edit an args JSON scope in the TUI); expiry sweeper reclaims timed-out tickets
- **Sub-agent delegation** — frozen permission boundaries + resource limits; unified agent tree with `send_message` / `wait_agent` / `interrupt_agent` / `list_agents`
- **Context management** — auto window detection, percentage-triggered compaction (protocol-verified before install), oversized-result blob offload, CJK-aware prompt budget
- **exec resource limits** — memory (RLIMIT_AS) / CPU / file size / process count rlimits + setsid process-group isolation with whole-tree kill on timeout; credential env vars auto-stripped
- **Multi-provider** — OpenAI Responses, Chat Completions (DeepSeek/Qwen thinking mode), Anthropic Messages; failover switches credential AND endpoint per candidate
- **Credential Broker** — master credentials hidden from the agent; one-shot leases prevent replay; MCP OAuth authorization-code CLI
- **Long-term memory** — SQLite + FTS5 hybrid retrieval (BM25 + vector RRF), periodic re-indexing, prompt injection with an injection-defense frame
- **MCP support** — stdio JSON-RPC with 60s timeouts, id-correlated responses, out-of-order buffering, OAuth authorization flow
- **TUI** — Vim-style modal interface: thinking panel, sub-agent cards, approval cards (with args editor), mid-stream input becomes Steer

## Quick Start

### Prerequisites

- **Rust 1.85+** (edition 2024)
- An API key from your model provider

### Install & Run

```bash
git clone <this-repository>
cd grodex
cargo build --release

# Interactive REPL session
./target/release/grodex run --trusted

# Terminal UI (auto-spawns a `grodex serve` subprocess)
./target/release/grodex tui

# ACP server over stdio
./target/release/grodex serve

# Resume a crashed session
./target/release/grodex resume <session-id>
```

### Configuration

Create `~/.grodex/config.toml`:

```toml
provider = "openai"
model = "gpt-5"
wire_protocol = "responses"   # "responses" | "chat" | "messages"

# Sandbox
sandbox_profile = "workspace"  # "workspace" | "readonly" | "restricted" | "full"

# Permission rules
[rules]
read_file = "allow"
web_fetch = "allow"
write_file = "ask"
exec = "ask"
apply_patch = "ask"

# exec resource limits (optional)
# exec_memory_limit_mb = 8192
# exec_cpu_limit_secs = 600

# Multi-provider failover
# [model_routes.default]
# ...see config.example.toml for the full example
```

> Full configuration (provider failover, MCP servers, memory, compaction thresholds, etc.) in [config.example.toml](config.example.toml).

### Environment Variables

| Variable | Description |
|---|---|
| `OPENAI_API_KEY` | OpenAI API key (also `ANTHROPIC_API_KEY` etc.) |
| `GRODEX_PROVIDER` / `GRODEX_MODEL` / `GRODEX_WIRE_PROTOCOL` / `GRODEX_API_ENDPOINT` | Override provider / model / protocol / endpoint |
| `GRODEX_TELEMETRY_DB` | Telemetry DB path (default `~/.grodex/telemetry.db`) |
| `GRODEX_TELEMETRY_RETENTION_DAYS` | Telemetry retention in days (default 30, 0 disables) |
| `GRODEX_MEMORY_DB` | Memory DB path (default `~/.grodex/memory.db`) |
| `GRODEX_MEMORY_RESCAN_SECS` | Memory re-index interval (default 600s, 0 disables) |
| `GRODEX_PROMPT_BUDGET_TOKENS` | System prompt token budget (default 40000, CJK-aware estimate) |
| `GRODEX_LOG_DIR` | Log directory (default `~/.grodex/logs`) |

## CLI Commands

| Command | Description |
|---|---|
| `grodex run` | Interactive REPL session |
| `grodex tui` | Terminal UI |
| `grodex serve` | ACP server (stdio) |
| `grodex resume <id>` | Resume a session (heals interrupted tools, rebuilds the sub-agent tree) |
| `grodex replay / inspect / dump <id>` | Replay / inspect events / raw export |
| `grodex eval <id>` | Memory retrieval quality evaluation |
| `grodex prompt explain / dump` | Inspect system prompt assembly (redacted by default) |
| `grodex telemetry sessions / turn <id> / slow-tools / slow-models / cache / errors / recovery / doctor / timeline / vacuum / export` | Telemetry queries & ops |
| `grodex mcp-auth <server>` | MCP OAuth authorization (credentials persisted) |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        grodex-cli / grodex-tui                   │
│              (Interactive TUI · ACP client · telemetry CLI)      │
├─────────────────────────────────────────────────────────────────┤
│                        grodex-protocol (ACP)                     │
│              EventEnvelope · SessionSnapshot · stdio             │
├─────────────────────────────────────────────────────────────────┤
│                        grodex-loop                               │
│   SessionSupervisor → TurnCoordinator → SamplingStep → Reducer  │
│        RolloutWriter (JSONL Journal) ──► TelemetrySink           │
├──────────────┬──────────────┬───────────────┬───────────────────┤
│ grodex-      │ grodex-      │ grodex-       │ grodex-           │
│ sampler      │ provider     │ capability    │ subagent          │
│ (HTTP +      │ (canonical   │ (Prepared     │ (delegation       │
│  streaming)  │  IR +        │  Call fences) │  envelopes +      │
│              │  failover)   │               │  collaboration)   │
├──────────────┴──────────────┴───────────────┴───────────────────┤
│  grodex-tools · grodex-permission · grodex-sandbox · grodex-auth │
├─────────────────────────────────────────────────────────────────┤
│  grodex-prompt · grodex-memory · grodex-config · grodex-skills  │
│      grodex-mcp · grodex-rollout · grodex-telemetry              │
└─────────────────────────────────────────────────────────────────┘
```

<details>
<summary><strong>22 crates</strong> — click to expand the project layout</summary>

```
grodex/
├── crates/
│   ├── grodex-core/            # Shared types: ContextItem, IDs, PolicyDecision
│   ├── grodex-loop/            # Agent Loop: Supervisor, TurnCoordinator, Reducer
│   ├── grodex-provider/        # Canonical request/event model, protocol descriptors
│   ├── grodex-sampler/         # HTTP client, streaming decoders (3 protocols), failover
│   ├── grodex-capability/      # Capability descriptors, PreparedCapabilityCall
│   ├── grodex-permission/      # Policy engine, approval broker, leases, session grants
│   ├── grodex-sandbox/         # Seatbelt enforcement, path/network validation, limits
│   ├── grodex-tools/           # Built-in tools: read/write/edit/exec/patch/web_fetch/...
│   ├── grodex-subagent/        # Sub-agent tree, delegation envelopes, mailbox, protocol
│   ├── grodex-auth/            # Credential broker, secret stores, MCP OAuth
│   ├── grodex-config/          # TOML config, layered merge, hot-reload pipeline
│   ├── grodex-protocol/        # ACP types, EventEnvelope, stdio transport
│   ├── grodex-skills/          # Skill catalog, progressive disclosure, trust markers
│   ├── grodex-mcp/             # MCP client, JSON-RPC process management, OAuth coordinator
│   ├── grodex-memory/          # SQLite + FTS5 memory store and retrieval
│   ├── grodex-prompt/          # Four-zone prompt assembly, discovery, budget trimming
│   ├── grodex-rollout/         # JSONL journal single-writer actor, crash recovery
│   ├── grodex-telemetry/       # SQLite telemetry projection, queries, retention
│   ├── grodex-cli/             # CLI entry: run/serve/resume/telemetry/mcp-auth/...
│   └── grodex-tui/             # Terminal UI (ratatui + crossterm)
├── docs/                       # Design docs (14)
└── config.example.toml
```

</details>

## Design Invariants

Grodex enforces **17 runtime invariants** — not just documented, but verified in tests:

| # | Invariant |
|---|---|
| 1 | Session state transitions are serialized through the Supervisor |
| 2 | At most one Turn is admitted at a time |
| 3 | The model can only call tools exposed by the current StepSnapshot |
| 4 | Tool calls bind to a capability generation at dispatch time |
| 5 | No side effect before permission clears |
| 6 | Tool results commit in model emission order |
| 7 | A result is durable before the next sampling step |
| 8 | Cancellation waits for cleanup before admitting a new Turn |
| 9 | Compaction leaves no dangling tool calls |
| 10 | Memory snapshot is stable within a Turn |
| 11 | A background task finishing ≠ the main agent has read it |
| 12 | Sub-agent permission ≤ parent ceiling |
| 13 | `rollout.jsonl` is the single source of truth |
| 14 | Late events are rejected by generation comparison |
| 15 | Tools / Skills / MCP are frozen within a Turn |
| 16 | Revocation only tightens (epoch is monotonic) |
| 17 | AppOnly actions are journaled |

## TUI

Vim-style modal interface with a thinking panel, sub-agent cards, and an integrated approval workflow:

| Key | Action |
|---|---|
| `i` / `:` | Enter input / command mode |
| `Enter` | Send prompt (mid-stream becomes Steer) |
| `Ctrl`+`O` | Toggle thinking (CoT) panel |
| `Ctrl`+`E` | Toggle sub-agent execution log |
| `↑`/`↓` + `Enter` | Approval options: Allow / **Always allow** / Deny / Cancel / **Narrow (args editor)** |
| `Ctrl`+`C` | Cancel generation (twice to quit) |

## Tests

```bash
# Run the full suite (919 tests)
cargo test --workspace

# Run a single crate
cargo test -p grodex-loop
```

Coverage highlights: crash recovery (6 scenarios), generation fences, concurrent scheduling, golden wire-event replay (3 protocols), sandbox kernel-deny paths, delegation ceilings, credential lease replay, telemetry projection & idempotent re-projection, approval narrow/expiry/session grants.

## Documentation

Deep design documents live in `docs/`:

| Doc | Topic |
|---|---|
| `docs/09-agent-loop-v2-design.md` | Supervisor → TurnCoordinator → SamplingStep |
| `docs/11-context-management-v2-design.md` | Rollout journal, compaction, projection |
| `docs/14-provider-model-adapter-v2-design.md` | Canonical events, wire protocol decoding, failover |
| `docs/15-built-in-tools-v2-design.md` | Tool specs and contracts |
| `docs/16-permission-approval-execution-v2-design.md` | Policy engine, approvals, leases, session grants |
| `docs/17-frontend-acp-protocol-v2-design.md` | Event envelopes, streaming, resync |
| `docs/21-telemetry-observability-design.md` | Two-layer recording, SQLite projection, queries & ops |

## Contributing

Contributions welcome! Please:

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Make sure tests pass (`cargo test --workspace`)
4. Commit your changes (`git commit -m 'feat: add amazing feature'`)
5. Open a Pull Request

## License

Dual-licensed under MIT or Apache-2.0, at your option.

---

<div align="center">

**Built with Rust — designed for safety, transparency, and recoverability.**

</div>
