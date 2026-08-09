#![cfg(not(loom))]

//! Grodex Loop — Agent Loop runtime with three-actor architecture.
//!
//! Architecture (following Grok):
//!   SessionSupervisor — tokio::select! event loop
//!   ChatStateActor — exclusive transcript owner
//!   TurnCoordinator — turn lifecycle, parallel tool dispatch

pub mod capability;
pub mod capability_manager;
pub mod chat_state;
pub mod delegate_tool;
pub mod command;
pub mod context;
pub mod context_projection;
pub mod durable_subagent;
pub mod reducer;
pub mod rollout_writer;
pub mod session;
pub mod step_context;
pub mod step;
pub mod supervisor;
pub mod turn;
pub mod turn_coordinator;

pub use chat_state::{ChatStateActor, ChatStateHandle};
pub use command::{SessionCommand, SessionEvent};
pub use context::CompactionManager;
pub use session::Session;
pub use step::StepRunner;
pub use step_context::StepContext;
pub use supervisor::{SessionHandle, SessionSupervisor};
pub use step::TurnOutcome;
pub use turn::{StepResult, Turn, TurnContext};
pub use turn_coordinator::TurnCoordinator;
