//! Strongly-typed identifier newtypes for the Grodex agent.
//!
//! Every identifier in the system is a newtype wrapper — never a bare
//! `String` or `u64` — so that the compiler catches cross-wiring bugs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_uuid_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(Uuid);

        impl $name {
            /// Generate a new random identifier.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Parse from a canonical string representation.
            pub fn from_string(s: &str) -> Result<Self, crate::error::GrodexError> {
                Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|_| crate::error::GrodexError::InvalidId(s.to_string()))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

define_uuid_id!(SessionId, "A long-lived conversation session.");
define_uuid_id!(TurnId, "One user goal including multiple model samples.");
define_uuid_id!(StepId, "One model sample and its resulting tool batch.");
define_uuid_id!(StepSnapshotId, "Immutable config snapshot version used by a Step.");
define_uuid_id!(MemorySnapshotId, "Memory retrieval result version used by a Turn.");
define_uuid_id!(ToolCallId, "Model-generated tool call identifier.");
define_uuid_id!(OperationId, "Idempotency key for side-effecting operations.");

// ── Numeric newtypes ──────────────────────────────────────────────

/// Monotonic counter incremented on compaction/recovery to isolate stale events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StepGeneration(u64);

impl StepGeneration {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn initial() -> Self {
        Self(0)
    }

    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl Default for StepGeneration {
    fn default() -> Self {
        Self::initial()
    }
}

/// Ordered position of a ToolCall in the model response.
///
/// Lower values come first. Used for deterministic transcript ordering
/// regardless of per-tool execution completion time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CommitSequence(u64);

impl CommitSequence {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_ids_are_unique() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn uuid_ids_roundtrip_serialization() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let id2: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn uuid_ids_parse_display_roundtrip() {
        let id = TurnId::new();
        let s = id.to_string();
        let id2 = TurnId::from_string(&s).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn commit_sequence_ordering() {
        let a = CommitSequence::new(1);
        let b = CommitSequence::new(2);
        assert!(a < b);
    }

    #[test]
    fn step_generation_increments() {
        let g = StepGeneration::initial();
        assert_eq!(g.next().as_u64(), 1);
    }
}
