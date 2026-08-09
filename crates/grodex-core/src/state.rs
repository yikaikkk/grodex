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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallState {
    /// Model output has been parsed into a typed ToolCall.
    Parsed,
    /// Arguments validated against the tool's JSON Schema.
    Validated,
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
    /// Approval was denied.
    Rejected,
    /// The call was cancelled before resolution.
    Cancelled,
}
