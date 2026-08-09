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
    ToolCallPrepared,
    /// Execution of a tool started.
    ToolExecutionStarted,
    /// Execution of a tool finished (success or error).
    ToolExecutionFinished,
    /// A tool result was committed to the session transcript.
    ToolResultCommitted,
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
