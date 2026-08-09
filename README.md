# Grodex

[中文版](README.zh.md)

**Grodex** is an AI coding agent — a CLI tool and ACP server that reads, edits, and runs code in your project. It is written in Rust and designed around a set of architectural invariants that make it auditable, recoverable, and safe to sandbox.

- **Agent loop** with Session Supervisor + Turn Coordinator + Sampling Step
- **Rollout journal** (append-only event log) as the single source of truth for crash recovery
- **Built-in tools** (read / write / edit / exec / apply_patch) with permission and sandbox enforcement
- **Sub-agents** with delegation envelopes and authority ceilings
- **Credential broker** that never leaks master tokens
- **ACP protocol** with unified event envelopes for streaming and resync
- **macOS Seatbelt** kernel-enforced sandboxing (Linux Landlock + bubblewrap stubs ready for integration)

---

## Quick Start

### Prerequisites

- **Rust 1.85+** (edition 2024)
- An API key for your model provider (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc.)

### Build & Run

```bash
# Build from source
cd grodex
cargo build --release

# Start an interactive session
cargo run -- run

# Resume a previous session from its rollout journal
cargo run -- resume <session-id>

# Replay a session (prints the full conversation)
cargo run -- replay <session-id>

# Run as an ACP server over stdio (for IDE integration)
cargo run -- serve

# Show version
cargo run -- version
```

### Configuration

Create `~/.grodex/config.toml` or `<project>/.grodex/config.toml`:

```toml
provider = "openai"
model = "gpt-5"
wire_protocol = "responses"   # "responses" | "chat" | "messages"
# endpoint = "https://api.openai.com/v1"
# api_key = "sk-..."          # prefer env var OPENAI_API_KEY

# ── Sandbox ───────────────────
sandbox_profile = "workspace"  # "workspace" | "readonly" | "restricted" | "full"

# ── Permission rules ──────────
# [rules]
# read_file = "allow"
# write_file = "ask"
# exec = "ask"

# ── Memory ────────────────────
# [memory]
# enabled = true
# path = "~/.grodex/memory.json"
```

See `config.example.toml` for multi-provider failover routes and detailed options.

### Environment Variables

| Variable | Description |
|---|---|
| `OPENAI_API_KEY` | API key for OpenAI provider |
| `ANTHROPIC_API_KEY` | API key for Anthropic provider |
| `GRODEX_PROVIDER` | Override the provider name |
| `GRODEX_MODEL` | Override the model name |
| `GRODEX_WIRE_PROTOCOL` | Override wire protocol (`responses`/`chat`/`messages`) |
| `GRODEX_API_ENDPOINT` | Override API endpoint URL |

---

## Architecture

Grodex is a Rust workspace of **25 crates** organised into four layers:

### Core Loop (`grodex-loop`, `grodex-core`, `grodex-provider`, `grodex-sampler`)

- **SessionSupervisor** — `tokio::select!` event loop that multiplexes commands, turn completions, and timers.
- **TurnCoordinator** — a single Turn (= one user goal) runs multiple sampling steps, dispatches tools in parallel, commits results in model-emission order.
- **Canonical Model Request/Event** — provider-agnostic intermediate representation. Streaming decoders for Responses / Chat Completions / Messages APIs produce canonical events.
- **Rollout Journal** — every state change is recorded in `~/.grodex/sessions/{id}/rollout.jsonl` (append-only JSONL + content-addressed blob store). The `SessionReducer` replays events to rebuild the transcript on crash recovery.

### Capability & Permission (`grodex-capability`, `grodex-permission`, `grodex-sandbox`, `grodex-tools`)

- **5 built-in tools**: `read_file`, `write_file`, `edit_file`, `exec`, `apply_patch`. Each tool declares its concurrency class, side-effect class, and default policy.
- **Permission pipeline**: static policy evaluation → approval ticket → user resolution → permission lease → execution. All rules use **strictest-merge** semantics (Deny > Ask > Allow, priority only breaks ties).
- **Sandbox enforcement**: on macOS, Seatbelt profiles are actually applied via `sandbox-exec` (kernel-enforced, not just a generated string). Linux Landlock and bubblewrap stubs are ready.
- **PreparedCapabilityCall**: at dispatch time every tool call is bound to an immutable snapshot (capability revision, policy generation, validated args, SHA-256 of arguments).

### Sub-Agents & Delegation (`grodex-subagent`)

- **SubAgentSupervisor** monitors child agents, enforces timeouts, cascades cancellations.
- **DurableSubAgentSupervisor** journals every spawn/complete/fail/cancel through the shared `RolloutWriter` so a restarted session can recover.
- **DelegationEnvelope** freezes the authority boundary a parent hands to a child: capability subset, policy ceiling, sandbox profile, resource budget, authority ceiling. `authorize_tool_call` enforces invariant #12 (child authority ≤ parent).

### Protocol & UI (`grodex-protocol`, `grodex-cli`, `grodex-tui`)

- **ACP (Agent Client Protocol)** over JSON-RPC on stdio. Supports `initialize`, `session/new`, `session/load`, `session/prompt`, `session/cancel`, `ResolveApproval`, `ResumeSession`.
- **EventEnvelope** wraps every streaming update with seq, event_id, parent_event_id, causation_token, and generation — enabling gap detection, UI stitching, and exactly-once replay.
- **SessionSnapshot** for fast resync after disconnect.

### Auth & Credentials (`grodex-auth`, `grodex-auth-types`)

- **CredentialBroker** is the trusted holder of master tokens. Agents never see the raw token — they redeem single-use `CredentialLease`s via `broker.resolve()`. Leases are endpoint-bound, epoch-gated, and consumed on first use (anti-replay).
- Optional macOS Keychain backing for durable token storage across restarts.

### Skills, MCP, Memory, Config, Prompt (`grodex-skills`, `grodex-mcp`, `grodex-memory`, `grodex-config`, `grodex-prompt`)

- **Skills** catalog with filesystem discovery and YAML/Toml manifests.
- **MCP client** spawns MCP server processes and communicates via stdio JSON-RPC (`tools/list`, `call_tool`).
- **Memory** keyword + tag-based retrieval with importance ranking, formatted for system-prompt injection.
- **Config** layered resolution: system → enterprise → user → profile → workspace, with merge traces for audit.
- **Prompt assembly**: instruction priority zones A→C→B→D, with `PromptBuilder` + `InstructionDiscovery` for AGENTS.md / CLAUDE.md auto-load.

---

## Project Layout

```
grodex/
├── crates/
│   ├── grodex-core/            # Shared types: ContextItem, IDs, PolicyDecision, error
│   ├── grodex-loop/            # Agent loop: Supervisor, TurnCoordinator, Reducer, RolloutWriter
│   ├── grodex-provider/        # Canonical request/event model, wire protocol descriptors
│   ├── grodex-sampler/         # HTTP client, streaming decoders (Responses/Chat/Messages)
│   ├── grodex-capability/      # Capability descriptors, step snapshots, PreparedCapabilityCall
│   ├── grodex-permission/      # Policy engine, approval broker, resolution, permission leases
│   ├── grodex-sandbox/         # Profile store, path validator, Seatbelt enforcement, runtime
│   ├── grodex-sandbox-types/   # SandboxProfile, SandboxBinding shared types
│   ├── grodex-tools/           # Built-in tools: read, write, edit, exec, patch
│   ├── grodex-subagent/        # Sub-agent tree, task lifecycle, delegation envelopes
│   ├── grodex-auth/            # Credential broker, auth manager, secret store
│   ├── grodex-auth-types/      # Account descriptors, credential handles, lease types
│   ├── grodex-rollout/         # RolloutEvent types, FileRolloutStore (JSONL + blobs)
│   ├── grodex-config/          # TOML config loader, layered merge, requirements validation
│   ├── grodex-protocol/        # ACP types, EventEnvelope, SessionSnapshot, stdio transport
│   ├── grodex-skills/          # Skill catalog, filesystem discovery
│   ├── grodex-mcp/             # MCP client, JSON-RPC process management
│   ├── grodex-memory/          # Memory store, keyword/tag retriever
│   ├── grodex-prompt/          # Prompt builder, instruction assembly
│   ├── grodex-cli/             # CLI entry point: run, serve, resume, replay
│   └── grodex-tui/             # Terminal UI (ratatui + crossterm)
├── docs/                       # v2 design documents (13 files)
├── task/                       # Implementation gap audit and tracking
├── config.example.toml
├── Cargo.toml                  # Workspace manifest
└── README.md
```

---

## Design Principles

The agent is built around a set of **immutable invariants** enforced at runtime and verified in tests:

| # | Invariant |
|---|---|
| 1 | Session control-state transitions are serialised through the Supervisor |
| 2 | At most one Turn is admitted at a time |
| 3 | Model may only call tools exposed by the current StepSnapshot |
| 4 | Tool calls are bound to a capability revision at dispatch time |
| 5 | No side effect before the permission gate clears |
| 6 | Tool results are committed in model-emission order |
| 7 | A tool result must be durable before the next sampling step |
| 8 | Cancellation waits for cleanup before a new Turn is admitted |
| 9 | Compaction leaves no dangling tool calls |
| 10 | Memory context snapshot is stable within a Turn |
| 11 | Background task completion ≠ main agent has read it |
| 12 | Child agent authority ⊈ parent ceiling |
| 13 | rollout.jsonl is the single source of truth |
| 14 | Late events are rejected via generation comparison |
| 15 | Tool / Skill / MCP are stable within a Turn |
| 16 | Revocation only tightens (epoch monotonic) |
| 17 | AppOnly actions are recorded in the rollout |

---

## Testing

```bash
# Run all tests (~234, 0 failures)
cargo test --workspace

# Run a specific crate
cargo test -p grodex-loop

# Run with backtrace
RUST_BACKTRACE=1 cargo test --workspace
```

Key test categories:
- **Recovery**: 6 crash-point recovery tests (before sampling, mid-stream, before/after tool result, before compaction, before TurnCompleted).
- **Fence**: generation regression detection, commit fence on store failure.
- **Scheduling**: randomised concurrent tool completion with deterministic model-order commit.
- **Golden**: wire-event fixture replay for all three wire protocols.
- **Seatbelt**: macOS kernel-enforced deny-path test.
- **Delegation**: authority ceiling, policy ceiling, tool subset, revocation checks.
- **Lease**: single-use redemption, anti-replay, audience mismatch, epoch revocation.

---

## Stdin Commands (Interactive Mode)

| Command | Description |
|---|---|
| `/quit`, `/exit` | Exit the session |
| `/help` | Show help |
| `/compact` | Trigger context compaction |

---

## License

MIT OR Apache-2.0

---

## Related Documents

- `docs/09-agent-loop-v2-design.md` — Agent loop architecture
- `docs/11-context-management-v2-design.md` — Rollout journal and context projection
- `docs/14-provider-model-adapter-v2-design.md` — Provider adapter and canonical events
- `docs/15-built-in-tools-v2-design.md` — Built-in tool specifications
- `docs/16-permission-approval-execution-v2-design.md` — Permission and approval system
- `docs/17-frontend-acp-protocol-v2-design.md` — ACP protocol specification
- `task/grodex-implementation-gap-audit.md` — Implementation audit and gap tracking
