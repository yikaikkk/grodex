//! TaskRun — one execution of an agent with input, budget, status, and result.

use crate::context::ContextFork;
use crate::node::AgentId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a task run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TaskId(Uuid);

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

/// Status of a task run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    /// Task created but not yet started.
    Pending,
    /// Task is executing.
    Running,
    /// Task completed successfully.
    Completed,
    /// Task failed with an error.
    Failed,
    /// Task was cancelled.
    Cancelled,
}

/// Budget limits for a task run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBudget {
    /// Maximum number of model sampling turns.
    pub max_turns: Option<u32>,
    /// Maximum wall-clock duration in seconds.
    pub max_duration_secs: Option<u64>,
}

/// One execution of an agent.
///
/// The task carries the input prompt, context fork, budget, and status.
/// Results are written when the task completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: TaskId,
    /// Which agent executes this task.
    pub agent_id: AgentId,
    /// The instruction/prompt for this task.
    pub input: String,
    /// What context the agent inherits.
    pub context_fork: ContextFork,
    /// Resource budget.
    pub budget: TaskBudget,
    /// Current status.
    pub status: TaskStatus,
    /// Result text (populated on completion).
    pub result: Option<String>,
    /// Error message (populated on failure).
    pub error: Option<String>,
    /// Token usage for this task.
    pub tokens_used: Option<u64>,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// When the task finished.
    pub finished_at: Option<DateTime<Utc>>,
}

impl TaskRun {
    /// Create a new task.
    pub fn new(agent_id: AgentId, input: impl Into<String>, context_fork: ContextFork, budget: TaskBudget) -> Self {
        Self {
            id: TaskId::new(),
            agent_id,
            input: input.into(),
            context_fork,
            budget,
            status: TaskStatus::Pending,
            result: None,
            error: None,
            tokens_used: None,
            created_at: Utc::now(),
            finished_at: None,
        }
    }

    /// Mark the task as completed with a result.
    pub fn complete(&mut self, result: String, tokens: u64) {
        self.status = TaskStatus::Completed;
        self.result = Some(result);
        self.tokens_used = Some(tokens);
        self.finished_at = Some(Utc::now());
    }

    /// Mark the task as failed with an error.
    pub fn fail(&mut self, error: impl Into<String>) {
        self.status = TaskStatus::Failed;
        self.error = Some(error.into());
        self.finished_at = Some(Utc::now());
    }

    /// Mark the task as cancelled.
    pub fn cancel(&mut self) {
        self.status = TaskStatus::Cancelled;
        self.finished_at = Some(Utc::now());
    }

    /// Whether the task has reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}
