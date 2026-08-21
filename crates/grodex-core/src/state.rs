//! State machine enums for Session, Turn, and Tool Call lifecycles.
//!
//! These are bare enums — embedding the *complete* state machine in the
//! type system. Domain crates may wrap these with additional data, but
//! the core variants are canonical.

use serde::{Deserialize, Serialize};

/// Top-level session lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    /// Loading transcript, restoring journal, connecting MCP, building runtime.
    Initializing,
    /// Ready to accept a new Turn.
    Idle,
    /// A foreground Turn is in progress; external events are still handled.
    Running,
    /// No new Turns accepted; existing tasks converging toward shutdown.
    ShuttingDown,
    /// All resources released.
    Closed,
}

/// A single user-goal lifecycle within a Session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnState {
    /// Input has been received but work has not yet started.
    Admitted,
    /// Memory, Skill, and capability preparation in progress.
    Preparing,
    /// Model is being sampled.
    Sampling,
    /// Tool batch dispatched; awaiting execution results.
    AwaitingTools,
    /// Context is being compacted mid-Turn.
    Compacting,
    /// Turn result is being assembled.
    Finalizing,
    /// Turn finished successfully.
    Completed,
    /// Turn was explicitly cancelled.
    Cancelled,
    /// Turn ended with an error.
    Failed,
}

/// One tool call's journey from model output to final disposition.
///
/// The full lifecycle (Design Doc 16):
///   Parsed → Validated → [PolicyDenied | PolicyAllowed → AwaitingApproval → Approved]
///          → Running → Completed → Committed
///          → [Rejected | Cancelled | RolledBack | Superseded]
///
/// Invariants:
///   - PolicyDenied can only follow Validated (policy refused before approval).
///   - PolicyAllowed can only follow Validated (policy auto-approved).
///   - Committed follows Completed — the result is durable on disk.
///   - RolledBack / Superseded are terminal states from recovery/compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallState {
    /// Model output has been parsed into a typed ToolCall.
    Parsed,
    /// Arguments validated against the tool's JSON Schema.
    Validated,
    /// Policy evaluation denied the call (before human approval).
    /// The call will not proceed to AwaitingApproval.
    PolicyDenied,
    /// Policy evaluation auto-approved the call (no human needed).
    /// Skips AwaitingApproval and proceeds directly toward execution.
    PolicyAllowed,
    /// Waiting for user or policy approval.
    AwaitingApproval,
    /// Approved; ready to execute.
    Approved,
    /// Tool runtime is executing the call.
    Running,
    /// Execution finished successfully.
    Completed,
    /// Execution finished with an error.
    Failed,
    /// Approval was denied by user or policy.
    Rejected,
    /// The call was cancelled before resolution.
    Cancelled,
    /// Result has been durably committed to the journal.
    /// After this state, the result is visible to the next sampling step.
    Committed,
    /// The call was rolled back during crash recovery (side-effect
    /// fate was indeterminate and human resolved as "not executed").
    RolledBack,
    /// The call was superseded by a compaction or replacement.
    /// The original result is no longer in the active projection.
    Superseded,
}

impl ToolCallState {
    /// Whether this state is a terminal state (no further transitions).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Committed
                | Self::Rejected
                | Self::Cancelled
                | Self::RolledBack
                | Self::Superseded
        )
    }

    /// Whether this state represents a successful outcome.
    pub fn is_success(self) -> bool {
        matches!(self, Self::Completed | Self::Committed)
    }

    /// Whether the call has passed the policy gate (either auto-approved
    /// or sent to human approval).
    pub fn has_passed_policy(self) -> bool {
        matches!(
            self,
            Self::PolicyAllowed
                | Self::AwaitingApproval
                | Self::Approved
                | Self::Running
                | Self::Completed
                | Self::Committed
        )
    }

    /// Whether the call is still in a pre-execution state.
    pub fn is_pre_execution(self) -> bool {
        matches!(
            self,
            Self::Parsed
                | Self::Validated
                | Self::PolicyDenied
                | Self::PolicyAllowed
                | Self::AwaitingApproval
                | Self::Approved
        )
    }

    /// Valid state transitions. Returns true if `to` is a legal
    /// successor of `self`.
    pub fn can_transition_to(self, to: Self) -> bool {
        use ToolCallState::*;
        matches!(
            (self, to),
            // Forward lifecycle
            (Parsed, Validated)
                | (Validated, PolicyDenied)
                | (Validated, PolicyAllowed)
                | (Validated, AwaitingApproval)
                | (PolicyAllowed, Approved)
                | (AwaitingApproval, Approved)
                | (AwaitingApproval, Rejected)
                | (Approved, Running)
                | (Running, Completed)
                | (Running, Failed)
                | (Completed, Committed)
                // Cancellation can happen from any non-terminal state
                | (Parsed, Cancelled)
                | (Validated, Cancelled)
                | (PolicyDenied, Cancelled)
                | (AwaitingApproval, Cancelled)
                | (Approved, Cancelled)
                | (Running, Cancelled)
                // Recovery / compaction terminal states
                | (Completed, Superseded)
                | (Failed, RolledBack)
                | (Running, RolledBack)
        )
    }
}
