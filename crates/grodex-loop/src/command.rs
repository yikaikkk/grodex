//! Session commands and events — the message protocol between the CLI
//! (or any frontend) and the SessionSupervisor.

use grodex_core::id::TurnId;
use grodex_core::policy::PolicyDecision;

/// Commands sent to the SessionSupervisor from the frontend.
#[derive(Debug)]
pub enum SessionCommand {
    /// Start a new Turn with the given user input.
    StartTurn { user_input: String },
    /// Steer an in-progress Turn — modify the goal mid-execution.
    Steer { user_input: String },
    /// Cancel the currently running Turn.
    CancelTurn,
    /// Shut down the session gracefully.
    Shutdown,
    /// Resolve a pending approval ticket (Allow/Narrow/Deny/Cancel).
    /// `narrowed_args` is only meaningful for `Narrow`.
    ResolveApproval {
        ticket_id: String,
        decision: PolicyDecision,
        narrowed_args: Option<serde_json::Value>,
    },
    /// Resume a session after a disconnect — the client tells us the
    /// last seq it processed; we emit a `SnapshotReady` event in response.
    ResumeSession {
        last_seq: u64,
        idempotency_key: Option<String>,
    },
}

/// Events emitted by the SessionSupervisor to the frontend.
///
/// Mirrors ACP `UpdateContent` 1-to-1 so the CLI layer can map each
/// variant to an EventEnvelope with zero information loss. The TUI
/// then uses the streamed payloads to render the reasoning panel,
/// the tool-call cards, and the assistant text all in real time.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A Turn has started.
    TurnStarted { turn_id: TurnId },
    /// Text content from the model (streaming chunk).
    TextDelta { text: String },
    /// Reasoning / thinking content from the model (streaming chunk).
    ReasoningDelta { text: String },
    /// A tool call is starting — maps to ACP ToolCallStart.
    ToolCallStart { call_id: String, name: String },
    /// Tool arguments are arriving incrementally — maps to ACP ToolCallArgs.
    ToolCallArgs { call_id: String, args_delta: String },
    /// Tool arguments are fully streamed — maps to ACP ToolCallEnd.
    ToolCallEnd { call_id: String },
    /// A tool has finished executing and produced output — maps to
    /// ACP ToolResult.
    ToolResult { call_id: String, content: String, is_error: bool },
    /// A Step completed.
    StepCompleted { turn_id: TurnId, text: String },
    /// A Turn reached a terminal state.
    TurnCompleted { turn_id: TurnId },
    /// An error occurred.
    Error { message: String },
    /// The session has shut down.
    Shutdown,
    /// A snapshot is ready for the client (in response to `ResumeSession`
    /// or generated on initial connect). Carries the JSON-serialized
    /// snapshot payload so the frontend can render without replaying.
    SnapshotReady {
        last_seq: u64,
        generation: u64,
        current_turn_id: Option<String>,
        items_json: String,
    },
    /// An approval ticket was resolved (acknowledgement to the frontend).
    ApprovalResolved {
        ticket_id: String,
        accepted: bool,
    },
}
