//! Immutable event log event types.
//!
//! Every state change in a Session is recorded as a `RolloutEvent`.
//! This is the source of truth for recovery, auditing, and replay.

use grodex_core::id::{SessionId, StepGeneration, StepId, TurnId};
use serde::{Deserialize, Serialize};

/// The type of a rollout event — determines the payload schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RolloutEventType {
    /// A new user message was accepted.
    UserInputAccepted,
    /// The model produced one or more content items.
    ModelItemProduced,
    /// A tool call was parsed and validated.
    ///
    /// Schema (payload):
    ///   - call_id: str          — per-invocation correlation key (matching 1:1 with a `ToolCall` object)
    ///   - operation_id: str?    — idempotency / side-effect key; cross-restart dedup happens on THIS key
    ///   - name: str             — tool name
    ///   - args: Value           — the full args JSON as prepared by the model (+ narrow)
    ///   - args_hash: str?       — SHA-256 of canonical(args); used to detect drift
    ///   - capability_revision: str?  — generation/snapshot of the capability registry at the time
    ///   - policy_generation: u64?    — version of the permission policy evaluated to reach Approved
    ToolCallPrepared,
    /// A prepared tool call was approved (either auto-approved or by a
    /// human broker response). This is the durable "go ahead" signal
    /// that the executor consumes immediately before spawning the tool.
    ///
    /// Schema: same keys as ToolCallPrepared (call_id + operation_id are
    /// required).
    ToolCallApproved,
    /// Execution of a tool started.
    ///
    /// Schema: call_id, operation_id?, name
    ToolExecutionStarted,
    /// Execution of a tool finished (success or error).
    ///
    /// Schema: call_id, operation_id?, is_error, content?, exit_code?, duration_ms?, output_truncated?
    ToolExecutionFinished,
    /// A tool result was committed to the session transcript.
    ///
    /// Schema: call_id, operation_id?, is_error, content
    ToolResultCommitted,
    /// The system recovered from a crash and cannot determine whether a
    /// started-but-not-finished side effect actually took place. Any
    /// execution of the call from this point on MUST first get a
    /// `ToolOutcomeResolved` event written by the human-resolution
    /// protocol.
    ///
    /// Schema: call_id, operation_id?, name, reason: str
    ToolOutcomeIndeterminate,
    /// A human (or external arbiter) resolved an indeterminate tool
    /// outcome.
    ///
    /// Schema:
    ///   - call_id, operation_id?
    ///   - resolution: "confirmed_executed" | "confirmed_not_executed" | "terminated"
    ///   - resolved_content?: str (result of the side-effect if confirmed)
    ///   - resolver_id?: str
    ///   - resolved_at: RFC3339 timestamp (redundant with event.timestamp, kept for UI convenience)
    ToolOutcomeResolved,
    /// Old context items were pruned from the projection.
    ProjectionPruned,
    /// Runtime state changed (e.g. Idle → Running).
    RuntimeStateChanged,
    /// A prompt snapshot was built for a sampling step.
    PromptSnapshotBuilt,
    /// Compaction started.
    CompactionStarted,
    /// A compaction candidate was assembled.
    CompactionCandidateBuilt,
    /// Compaction was committed to the context projection.
    CompactionCommitted,
    /// Compaction failed and was rolled back.
    CompactionFailed,
    /// A Turn reached a terminal state.
    TurnCompleted,
    /// A sub-agent task started (parent spawned a child + a TaskRun).
    SubAgentTaskStarted,
    /// A sub-agent task reached a terminal state (completed / failed / cancelled).
    SubAgentTaskFinished,
    /// A model route event (candidate selected / failed / succeeded / exhausted).
    ModelRouteEvent,
    /// A deferred capability was promoted to active (deferred promotion approved).
    CapabilityPromoted,
    /// A capability call was rejected because the bound snapshot was stale
    /// (superseded by a newer generation before execution).
    CapabilityCallRejectedStale,
    /// A user narrowed the execution scope of a tool call, creating an
    /// EffectiveToolCallRevision with effective_args.
    EffectiveToolCallRevisionCreated,
    /// Context items restored from a prior session journal during `resume`.
    /// Written at the start of a new session so a second crash does not lose
    /// the recovered history. The payload is a serialized `Vec<ContextItem>`.
    ContextRestored,
    /// The supervisor submitted a tool call for human/automated approval.
    /// Schema:
    ///   - call_id, operation_id?
    ///   - ticket_id: str  — stable identifier in the approval broker
    ///   - tool_name, args
    ///   - requested_by: "supervisor" | "model" | "human_delegate"
    ///   - requested_at: RFC3339
    ApprovalRequested,
    /// An outstanding approval ticket reached a terminal resolution
    /// (approved | rejected | expired).
    /// Schema:
    ///   - ticket_id
    ///   - call_id?
    ///   - resolution: "approved" | "rejected" | "expired" | "narrowed"
    ///   - resolved_by: str?
    ///   - narrowed_args?: Value  (only populated for resolution="narrowed")
    ApprovalResolved,
    /// The broker issued a one-time lease for a (call_id, ticket_id).
    /// The lease is valid exactly once; replaying the same lease after
    /// the session has already consumed it MUST be rejected.
    /// Schema:
    ///   - lease_id: str
    ///   - ticket_id: str
    ///   - call_id
    ///   - ttl_secs?: u64
    ///   - issued_at: RFC3339
    LeaseIssued,
    /// A lease was consumed by the executor to actually run the tool.
    /// Once this event is on disk, reissuance with the same lease_id is
    /// forbidden — the consumer is expected to verify
    /// `!is_lease_consumed(lease_id)` before taking the side-effect path.
    /// Schema: lease_id, call_id, consumed_at: RFC3339
    LeaseConsumed,
    /// A lease reached its TTL without a matching LeaseConsumed (or the
    /// owning ticket expired). Schema: lease_id, reason
    LeaseExpired,
    /// A Turn-level skill snapshot was frozen at Turn start.
    /// Records the skill name, source, path, and content_hash for
    /// each skill active during the Turn — enables version auditing
    /// and replay reproducibility (Design Doc 08 §6).
    /// Schema:
    ///   - skills: [{ name, source, path, content_hash }]
    ///   - skill_generation: u64
    SkillSnapshotRecorded,

    /// An application-initiated tool call that does NOT go through
    /// model sampling (invariant #17). AppOnly calls are produced by
    /// application logic (e.g. auto-fixup, system maintenance, or
    /// human-initiated side effects) and must still be recorded in
    /// the rollout journal for auditability and recovery.
    ///
    /// Schema:
    ///   - tool_name: str
    ///   - args: Value
    ///   - result: Value
    ///   - reason: str  (why this was AppOnly, e.g. "auto_compaction", "system_maintenance")
    ///   - operation_id: str?
    ///   - call_id: str
    AppOnlyToolCall,

    /// The process opened the session (durable session boundary — gives
    /// the telemetry projection a `sessions.started_at` anchor).
    /// Schema:
    ///   - cwd: str            (raw path; the telemetry projection stores
    ///                          only its SHA-256)
    ///   - model_provider: str
    ///   - model: str
    SessionStarted,
    /// A user Turn was admitted and started. **Durable** — this is the
    /// `turns.started_at` anchor; without it a crash between input
    /// acceptance and turn completion leaves the turn invisible to the
    /// projection. Schema: input_chars: usize
    TurnStarted,
    /// A user-minted "always allow" session grant (Doc 16 §15). Durable —
    /// replay restores the grant so "always allow" survives a restart.
    /// Schema: grant_id, tool_name, matcher (structured k=v list),
    /// policy_generation.
    SessionGrantCreated,
    /// An ephemeral steering note was injected into the live transcript
    /// (repair prompt after a no-tool stop, or the length-continuation
    /// note). **Non-durable** — journaled so a replayed context matches
    /// the live one exactly, but the payload is a synthetic user message,
    /// not real user input. Schema: note_kind ("repair"|"continuation"),
    /// content, role: "user".
    PromptInjected,
    /// One model sampling round started (observability-only, NOT
    /// durable). One round may internally perform several provider
    /// attempts (retry/failover) — those surface via ModelRouteEvent.
    /// Schema:
    ///   - request_id: str
    ///   - provider: str, model: str, wire_protocol: str
    ModelAttemptStarted,
    /// One model sampling round finished (observability-only, NOT
    /// durable). Schema:
    ///   - request_id: str
    ///   - attempts: u32            (provider attempts consumed)
    ///   - duration_ms: u64         (whole round incl. retries)
    ///   - status: "ok" | "error"
    ///   - error_class?: str        (SamplingError::kind_label)
    ///   - http_status?: u16
    ///   - retry_after_secs?: u64
    ///   - provider_request_id?: str
    ///   - usage?: { input_tokens, cached_input_tokens,
    ///               cache_creation_tokens, output_tokens,
    ///               reasoning_tokens, total_tokens, estimated }
    ModelAttemptFinished,
}

/// Sensitivity classification for an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SensitivityLevel {
    /// Normal event — safe to include in logs and debug output.
    Normal,
    /// Contains credentials or secrets — must not appear in debug logs.
    Credential,
    /// Contains personally identifiable information.
    Personal,
}

/// One immutable event in the session journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutEvent {
    /// Schema version for forward/backward compatibility.
    pub schema_version: u32,
    /// Monotonically increasing sequence number within the session.
    pub seq: u64,
    /// The session this event belongs to.
    pub session_id: SessionId,
    /// The active Turn at the time of the event, if any.
    pub turn_id: Option<TurnId>,
    /// The active Step at the time of the event, if any.
    pub step_id: Option<StepId>,
    /// The Step generation at the time of the event.
    pub generation: Option<StepGeneration>,
    /// Wall-clock timestamp.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Discriminator for the payload.
    pub event_type: RolloutEventType,
    /// Type-specific event data.
    pub payload: serde_json::Value,
    /// Sensitivity classification.
    pub sensitivity: SensitivityLevel,
}
