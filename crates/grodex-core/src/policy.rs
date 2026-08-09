//! Policy decision enum — the single type used across permission, approval, and execution.

use serde::{Deserialize, Serialize};

/// The three-tier policy decision used throughout the system.
///
/// Every tool call, file access, and network request routes through
/// a policy check that produces one of these decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicyDecision {
    /// The operation is permitted without user interaction.
    Allow,
    /// The operation requires explicit user approval before proceeding.
    Ask,
    /// The operation is blocked — do not execute.
    Deny,
}

impl PolicyDecision {
    /// Returns `true` if the decision permits execution (possibly after approval).
    pub fn is_executable(self) -> bool {
        matches!(self, Self::Allow | Self::Ask)
    }

    /// Returns `true` if the operation can proceed without waiting for the user.
    pub fn is_immediate(self) -> bool {
        matches!(self, Self::Allow)
    }
}
