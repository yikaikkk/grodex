//! Session commands and events — the message protocol between the CLI
//! (or any frontend) and the SessionSupervisor.

use grodex_core::context::ContextItem;
use grodex_core::id::{SessionId, TurnId};
use grodex_core::policy::PolicyDecision;
use grodex_rollout::store::RolloutStore;
use std::sync::Arc;

/// Human adjudication for an Indeterminate tool call.
#[derive(Debug, Clone)]
pub enum IndeterminateResolution {
    /// The user confirmed the side effect completed successfully.
    /// The provided content (if any) is recorded as the tool result.
    Succeeded,
    /// The user confirmed the side effect failed or was partial.
    /// The provided content (if any) is recorded as the error reason.
    Failed,
    /// Discard the indeterminate call — the model will re-issue it
    /// in a future Turn. No `ToolOutcomeResolved` event is written.
    Retry,
}

/// Commands sent to the SessionSupervisor from the frontend.
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
    /// Resolve an Indeterminate tool call discovered during crash recovery.
    ///
    /// When the journal replay finds a `ToolExecutionStarted` without a
    /// matching `ToolExecutionFinished`/`ToolResultCommitted`, the tool's
    /// side effect is in an unknown state. The supervisor writes a
    /// `ToolOutcomeIndeterminate` event and surfaces it to the frontend.
    /// The user then inspects the real-world state and sends this command
    /// to record the human adjudication:
    ///
    /// - `Succeeded` → the side effect completed successfully; write
    ///   `ToolOutcomeResolved` with the user-supplied content.
    /// - `Failed` → the side effect failed or was partial; write
    ///   `ToolOutcomeResolved` with an error note.
    /// - `Retry` → discard the indeterminate call; the model will re-issue
    ///   it in a future Turn (no `ToolOutcomeResolved` is written).
    ResolveIndeterminate {
        call_id: String,
        resolution: IndeterminateResolution,
        content: Option<String>,
    },
    /// Resume a session after a disconnect.
    ///
    /// The supervisor always rebuilds `Session.context + chat_state` from
    /// the attached rollout store so future turns carry history. For
    /// `emit_snapshot_to_frontend = true` it also broadcasts a
    /// `SessionEvent::SnapshotReady` so the UI renders the history — set
    /// this to `false` when the caller has already delivered a snapshot
    /// to the frontend (e.g. ACP main.rs reads a different session_id's
    /// store and writes ServerFrame::Snapshot directly) so we do not
    /// overwrite the frontend with a second, often empty, snapshot from
    /// the new session's still-empty journal.
    ResumeSession {
        last_seq: u64,
        idempotency_key: Option<String>,
        emit_snapshot_to_frontend: bool,
    },
    /// Rebuild session context from a recovered ContextItem vector.
    /// Used by the `/resume <id>` flow: the agent reads the old session's
    /// rollout journal, rebuilds the full context via SessionReducer, then
    /// sends this command so the supervisor's Session.context carries the
    /// restored history for future prompt building. The restored items are
    /// also persisted to the *new* session's journal as ContextRestored so
    /// a second crash does not lose the recovered history twice.
    RestoreContext {
        items: Vec<ContextItem>,
    },
    /// Swap the RolloutWriter's attached store + session id AND reseed
    /// the monotonic seq counter so future journal writes append to the
    /// *resumed* session's durable journal rather than leaking into the
    /// ephemeral "boot-new-session" empty directory.
    ///
    /// Sent by ACP `/resume <old_session_id>` immediately after
    /// `ResumeSession` + `RestoreContext`. Every outstanding clone of the
    /// writer (supervisor, coordinator, durable sub-agent) sees the same
    /// swap because they share a single `Arc<RwLock<Inner>>`.
    RebindRolloutWriter {
        new_store: Arc<dyn RolloutStore>,
        new_session_id: SessionId,
        /// Next event seq number to use (= last journal seq + 1). The
        /// writer's atomic counter is reseeded here, so subsequent
        /// commits append gap-free after the resumed journal tail.
        next_seq: u64,
    },
}

impl std::fmt::Debug for SessionCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionCommand::StartTurn { user_input } => f
                .debug_struct("StartTurn")
                .field("user_input_len", &user_input.len())
                .finish(),
            SessionCommand::Steer { user_input } => f
                .debug_struct("Steer")
                .field("user_input_len", &user_input.len())
                .finish(),
            SessionCommand::CancelTurn => write!(f, "CancelTurn"),
            SessionCommand::Shutdown => write!(f, "Shutdown"),
            SessionCommand::ResolveApproval { ticket_id, decision, narrowed_args: _ } => f
                .debug_struct("ResolveApproval")
                .field("ticket_id", ticket_id)
                .field("decision", decision)
                .finish_non_exhaustive(),
            SessionCommand::ResolveIndeterminate { call_id, resolution, content } => f
                .debug_struct("ResolveIndeterminate")
                .field("call_id", call_id)
                .field("resolution", resolution)
                .field("content_len", &content.as_ref().map(|c| c.len()).unwrap_or(0))
                .finish(),
            SessionCommand::ResumeSession { last_seq, idempotency_key, emit_snapshot_to_frontend } => f
                .debug_struct("ResumeSession")
                .field("last_seq", last_seq)
                .field("idempotency_key", idempotency_key)
                .field("emit_snapshot_to_frontend", emit_snapshot_to_frontend)
                .finish(),
            SessionCommand::RestoreContext { items } => f
                .debug_struct("RestoreContext")
                .field("items_count", &items.len())
                .finish(),
            // Arc<dyn RolloutStore> is not Debug; skip the opaque store handle.
            SessionCommand::RebindRolloutWriter { new_session_id, next_seq, new_store: _ } => f
                .debug_struct("RebindRolloutWriter")
                .field("new_session_id", new_session_id)
                .field("next_seq", next_seq)
                .field("new_store", &"<dyn RolloutStore>")
                .finish(),
        }
    }
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
    /// An Indeterminate tool call was discovered during crash recovery.
    /// The tool's `ToolExecutionStarted` event has no matching
    /// `ToolExecutionFinished`/`ToolResultCommitted` — the side effect
    /// is in an unknown state. The user must inspect the real-world
    /// state and send `ResolveIndeterminate` to adjudicate.
    IndeterminateToolCall { call_id: String, tool_name: String, message: String },
    /// A Step completed.
    StepCompleted { turn_id: TurnId, text: String },
    /// A Turn reached a terminal state.
    TurnCompleted { turn_id: TurnId },
    /// An error occurred.
    Error { message: String },
    /// Informational log (not an error, no UX alerting). Use this for
    /// success confirmations (resume, export, diagnostics, …) instead of
    /// abusing `Error` as a generic "toast". The frontend renders Info
    /// via push_log, not the red "Error" card.
    Info { message: String },
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
    /// A new approval ticket needs user attention.
    ///
    /// The PermissionManager emits this the moment `policy.evaluate()`
    /// returns `Ask` and a ticket lands in the broker. The frontend uses
    /// it to render a pending approval row with timeout countdown.
    ApprovalRequested {
        ticket_id: String,
        tool_name: String,
        summary: String,
        risk: String,
        timeout_remaining_ms: u64,
    },
}
