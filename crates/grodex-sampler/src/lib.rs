//! Grodex Sampler — streaming model sampling runtime.
//!
//! Provides:
//!   - `StreamingDecoder` trait + `ResponsesDecoder` — wire backend
//!   - `SamplingClient` — HTTP transport
//!   - `SamplingError` — production error taxonomy with rich classifiers
//!   - `RetryDecision` / `classify_error()` — pure retry classification
//!   - `CircuitBreaker` — Closed/Open/HalfOpen with sliding window
//!   - `RetryBudget` — per-request retry budget
//!   - `SamplingActor` — retry loop with progress tracking
//!   - `ModelRouteConfig` + `RouteEntry` — TOML multi-candidate routing table
//!   - `RouteSelector` — weighted selection with CompatibilityGate
//!   - `CompatibilityGate` — 7-dimension compatibility checks
//!   - `StreamFragment` — lightweight streaming payload the TUI wire
//!     consumes directly (avoids the sampler crate depending on ACP).

pub mod actor;
pub mod breaker;
pub mod client;
pub mod compat;
pub mod decoder;
pub mod error;
pub mod line_buffer;
pub mod retry;
pub mod route;
pub mod route_config;
pub mod route_selector;
pub mod streaming;
pub mod wire_types;

pub use actor::{SamplingActor, SamplingOutcome};
pub use breaker::{BreakerConfig, BreakerState, CircuitBreaker};
pub use client::{SamplingClient, SamplingClientConfig};
pub use compat::{CompatibilityGate, CompatibilityIssue};
pub use error::SamplingError;
pub use retry::{RetryBudget, RetryDecision, StreamProgress, classify_error, retry_backoff};
pub use route::{CandidateToml, ModelCandidate, ModelRoute, ModelRouteToml, RouteAttemptBudget, RouteEvent, StickyScope};
pub use route_config::{ModelRouteConfig, RouteConfigError, RouteEntry};
pub use route_selector::{HealthScore, RouteSelector, RouteStatus};
pub use streaming::{DecoderState, StreamingDecoder};

/// Fragments streamed from the sampler + tool runtime to the TUI frontend.
///
/// This enum is intentionally tiny and sampler-owned — it lets us send
/// reasoning + tool-call chunks + tool results over the same pipe as
/// assistant text, without pulling the full ACP protocol graph into the
/// sampler crate.
///
/// Consumers (loop::supervisor + cli + tui) map each variant to the
/// corresponding ACP `UpdateContent` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamFragment {
    /// Assistant text chunk (maps to `UpdateContent::TextDelta`).
    Text(String),
    /// Reasoning / "thinking" chunk (maps to `UpdateContent::ThoughtDelta`).
    Reasoning(String),
    /// Tool call just started (maps to `UpdateContent::ToolCallStart`).
    ToolCallStart {
        /// Stable id that links all fragments of the same call together.
        /// String-encoded (from `ToolCallId`) so downstream crates don't
        /// need to import the provider type.
        call_id: String,
        name: String,
    },
    /// Incremental JSON argument fragment (maps to
    /// `UpdateContent::ToolCallArgs`).
    ToolCallArgs { call_id: String, args_delta: String },
    /// Tool call finished streaming its arguments (maps to
    /// `UpdateContent::ToolCallEnd`).
    ToolCallEnd { call_id: String },
    /// Tool execution result. Emitted AFTER the tool runs inside the
    /// TurnCoordinator (this one is NOT produced by the sampling actor).
    /// Maps to `UpdateContent::ToolResult`.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// A tool call needs user approval. Emitted by the TurnCoordinator
    /// when `PermissionManager::check()` returns `Ask` and a ticket lands
    /// in the broker. Maps to `UpdateContent::RequestPermission` /
    /// `SessionEvent::ApprovalRequested`. This is the FIRST half of the
    /// approval round-trip (Design Doc 16 §10): ticket created → UI
    /// notified → user resolves → broker wakes the waiting tool.
    ApprovalRequested {
        ticket_id: String,
        tool_name: String,
        summary: String,
        risk: String,
        timeout_remaining_ms: u64,
        /// The tool call arguments — needed by the frontend to offer a
        /// Narrow (args-scope) approval.
        args: Option<serde_json::Value>,
    },
    /// Context compaction lifecycle status. Emitted by the TurnCoordinator
    /// around the summarization round-trip so the frontend can show a
    /// transient "会话压缩中…" indicator. `phase` ∈ {"started", "finished",
    /// "failed"}. Like `ToolResult`/`ApprovalRequested`, this is produced
    /// by the TurnCoordinator, not the sampling actor.
    CompactionStatus { phase: String },
}

