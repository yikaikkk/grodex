//! Base error type for the Grodex system.
//!
//! Every fallible operation returns `Result<T, GrodexError>`. Domain crates
//! may define their own error enums that convert into `GrodexError` via
//! `From` impls or the `Internal` variant.

/// Unified error type for the Grodex agent.
#[derive(Debug, thiserror::Error)]
pub enum GrodexError {
    /// An identifier string could not be parsed.
    #[error("invalid identifier: {0}")]
    InvalidId(String),

    /// A state transition was rejected (e.g. completing an already-cancelled Turn).
    #[error("invalid state transition: {reason}")]
    StateTransition {
        /// Human-readable explanation of why the transition was rejected.
        reason: String,
    },

    /// A tool's execution handler returned an error.
    #[error("tool execution failed: {0}")]
    ToolExecution(String),

    /// A configuration value was missing or malformed.
    #[error("configuration error: {0}")]
    Config(String),

    /// An approval ticket expired or was already resolved.
    #[error("approval error: {0}")]
    Approval(String),

    /// An I/O or infrastructure error that does not fit a more specific variant.
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),

    /// The rollout journal on disk contains a line that could not be
    /// parsed into a valid `RolloutEvent`. Fail-closed: callers must
    /// NOT silently skip this line and continue; either surface the
    /// error to the operator (repair the JSONL manually) or start a
    /// fresh session. Silent data loss is forbidden.
    #[error(
        "journal corrupt: path={path} line={line} reason={reason}"
    )]
    JournalCorrupt {
        path: String,
        line: u64,
        reason: String,
    },
}

impl GrodexError {
    /// Convenience constructor for state transition rejections.
    pub fn state_transition(reason: impl Into<String>) -> Self {
        Self::StateTransition { reason: reason.into() }
    }
}
