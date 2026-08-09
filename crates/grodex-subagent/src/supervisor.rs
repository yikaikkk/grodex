//! SubAgentSupervisor — monitors sub-agent lifecycle.
//!
//! Tracks running sub-agents, enforces timeouts, writes lifecycle
//! events to rollout, and cascades parent cancellation to children.

use crate::context::ContextFork;
use crate::manager::SubAgentManager;
use crate::node::AgentId;
use crate::task::{TaskBudget, TaskStatus};
use std::time::Duration;

/// Configuration for the sub-agent supervisor.
#[derive(Debug, Clone)]
pub struct SubAgentConfig {
    /// Maximum concurrent sub-agents.
    pub max_children: usize,
    /// Default task timeout.
    pub default_timeout: Duration,
    /// Whether to persist task state to rollout.
    pub persist_tasks: bool,
}

impl Default for SubAgentConfig {
    fn default() -> Self {
        Self {
            max_children: 5,
            default_timeout: Duration::from_secs(300),
            persist_tasks: true,
        }
    }
}

/// Supervises the lifecycle of sub-agents.
///
/// Created once per session. Monitors task timeouts, enforces
/// concurrency limits, and provides task state for recovery.
#[derive(Debug)]
pub struct SubAgentSupervisor {
    manager: SubAgentManager,
    config: SubAgentConfig,
    root_id: AgentId,
}

impl SubAgentSupervisor {
    /// Create a new supervisor.
    pub fn new(config: SubAgentConfig) -> Self {
        let mut manager = SubAgentManager::new(config.max_children);
        let root_id = manager.register_root();
        Self {
            manager,
            config,
            root_id,
        }
    }

    /// Get the root agent id.
    pub fn root_id(&self) -> AgentId {
        self.root_id
    }

    /// Spawn a child agent to execute a task.
    ///
    /// Returns the agent and task ids on success, or an error if
    /// the concurrency limit is exceeded.
    pub fn spawn(
        &mut self,
        parent_id: AgentId,
        label: &str,
        input: &str,
        context_fork: ContextFork,
        budget: Option<TaskBudget>,
    ) -> Result<(AgentId, crate::task::TaskId), String> {
        let budget = budget.unwrap_or(TaskBudget {
            max_turns: Some(5),
            max_duration_secs: Some(self.config.default_timeout.as_secs()),
        });

        self.manager.spawn(parent_id, label, input, context_fork, budget)
    }

    /// Check for timed-out tasks. Returns ids of tasks that should be cancelled.
    pub fn check_timeouts(&self) -> Vec<crate::task::TaskId> {
        let tree = self.manager.tree();
        let mut timed_out = Vec::new();

        for task in &tree.tasks {
            if task.status == TaskStatus::Running {
                if let Some(ref deadline) = task.budget.max_duration_secs {
                    let elapsed = chrono::Utc::now()
                        .signed_duration_since(task.created_at)
                        .num_seconds()
                        .max(0) as u64;
                    if elapsed >= *deadline {
                        timed_out.push(task.id);
                    }
                }
            }
        }

        timed_out
    }

    /// Complete a task with a result.
    pub fn complete_task(&mut self, task_id: &crate::task::TaskId, result: String, tokens: u64) {
        self.manager.complete_task(task_id, result, tokens);
    }

    /// Fail a task with an error.
    pub fn fail_task(&mut self, task_id: &crate::task::TaskId, error: &str) {
        self.manager.fail_task(task_id, error);
    }

    /// Cancel a sub-agent and all its descendants.
    pub fn cancel(&mut self, agent_id: &AgentId) {
        self.manager.cancel(agent_id);
    }

    /// Cancel all active sub-agents.
    pub fn cancel_all(&mut self) {
        self.manager.cancel_all();
    }

    /// Build a tree snapshot for the UI.
    pub fn tree(&self) -> crate::manager::AgentTree {
        self.manager.tree()
    }

    /// Number of active (non-terminal) tasks.
    pub fn active_tasks(&self) -> usize {
        self.manager.active_task_count()
    }

    /// Snapshot of task states for recovery/journal.
    pub fn task_snapshot(&self) -> Vec<crate::task::TaskRun> {
        self.manager.tree().tasks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_monitor() {
        let mut sup = SubAgentSupervisor::new(SubAgentConfig::default());
        let root = sup.root_id();

        let (_, task_id) = sup
            .spawn(root, "worker", "do work", ContextFork::None, None)
            .unwrap();

        assert_eq!(sup.active_tasks(), 1);
        sup.complete_task(&task_id, "done".into(), 100);
        assert_eq!(sup.active_tasks(), 0);
    }

    #[test]
    fn cancel_cascades() {
        let mut sup = SubAgentSupervisor::new(SubAgentConfig::default());
        let root = sup.root_id();

        let (child, _) = sup
            .spawn(root, "child", "task1", ContextFork::None, None)
            .unwrap();

        sup.spawn(child, "grandchild", "task2", ContextFork::None, None)
            .unwrap();

        assert_eq!(sup.active_tasks(), 2);
        sup.cancel(&child);
        assert_eq!(sup.active_tasks(), 0);
    }
}
