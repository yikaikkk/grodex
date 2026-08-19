//! Grodex Rollout — immutable event log and blob storage.
//!
//! Every Session writes a `rollout.jsonl` of `RolloutEvent` entries.
//! Large results go to a content-addressed blob store. Together they
//! provide the source of truth for recovery, auditing, and replay.

pub mod event;
pub mod journal_actor;
pub mod store;
pub mod fence;
pub mod gate;
pub mod invariants;
pub mod recovery;

pub use fence::{FenceError, GenerationFence};
pub use gate::{
    GateContext, MaxStepsGate, OpenTodosGate, StopDecision, TerminalAnswerGate,
    TerminationGate, TerminationGateEvaluated, evaluate_chain,
};
pub use invariants::{
    InvariantAssertion, InvariantFailure, InvariantReport, InvariantWarning,
    ToolCallLifecycleConsistency, StepGenerationMonotonic, CompactionAtomicity,
    NoCommittedBeforePrepared, TurnCompletionClosure, run_all_invariants,
};
pub use journal_actor::{FsyncPolicy, JournalHandle, replay_journal_strict};
pub use recovery::{ApprovalTicketResolution, RecoveryCheckpoint, ToolCallFate, recover_from_journal};
