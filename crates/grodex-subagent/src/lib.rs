//! Grodex Sub-agent — parent-child agent tree management.
//!
//! Provides AgentNode (stable identity), TaskRun (one execution),
//! ContextFork (context inheritance strategies), SubAgentManager
//! (tree lifecycle: spawn, cancel, track), and DelegationEnvelope
//! (the frozen security/authority boundary a parent hands a child).
//!
//! Also provides the infrastructure from Design Doc 12:
//! - AgentPath: tree-shaped addressing (`/root/reviewer/security`)
//! - Mailbox/MailboxRouter: persistent inter-agent messaging
//! - ResidencyManager: loaded/unloaded agent lifecycle with LRU eviction
//!   + live 4-state machine (Idle/Starting/Running/Exited) with tokens
//! - Scheduler: 10-dimensional resource governance + live async run loop
//! - WorkspaceManager: 4-mode file isolation + parallel write locks
//! - journal: RuntimeJournalHandle trait for SubAgentTaskStarted/Finished

pub mod context;
pub mod delegation;
pub mod journal;
pub mod mailbox;
pub mod manager;
pub mod node;
pub mod path;
pub mod protocol;
pub mod residency;
pub mod scheduler;
pub mod supervisor;
pub mod task;
pub mod workspace;

pub use context::ContextFork;
pub use delegation::{
    CapabilitySubset, DelegationBudget, DelegationEnvelope, DelegationEnvelopeBuilder,
    DelegationError,
};
pub use journal::{
    NoopJournalHandle, RuntimeJournalHandle, TaskJournalMeta, TaskResultSummary,
};
pub use mailbox::{
    AgentMessage, Mailbox, MailboxError, MailboxRouter, MessageId, MessageKind,
};
pub use manager::{AgentTree, SubAgentManager};
pub use node::{AgentId, AgentNode, AgentStatus};
pub use path::{AgentPath, PathError};
pub use protocol::{
    AgentListing, CollaborationProtocol, FollowupOutcome, MailboxReadResult, ProtocolConfig,
    ProtocolError, ProtocolEvent, WaitResult, WaitTargetState,
};
pub use residency::{
    ProcessInfo, ResidencyEntry, ResidencyError, ResidencyManager, ResidencyState,
    ResidencyToken, ResourceBudgetUsage, ResourcePool,
};
pub use scheduler::{
    ResourceBudget, ResourceLimitError, ResourceUsage, RunningTaskHandle, Scheduler,
    SchedulerDiagnostics, SchedulerError, SchedulerTickSummary, SpawnReservation,
};
pub use task::{TaskBudget, TaskId, TaskRun, TaskStatus};
pub use workspace::{
    WorkspaceError, WorkspaceLease, WorkspaceManager, WorkspaceMode, WriteLockGuard,
};
