//! SubAgentManager — central registry for agent nodes and task runs.
//!
//! Manages the agent tree: spawn new agents, track tasks, cancel running
//! work, and produce tree snapshots for the UI.

use crate::context::ContextFork;
use crate::node::{AgentId, AgentNode, AgentStatus};
use crate::task::{TaskBudget, TaskId, TaskRun};
use std::collections::HashMap;

/// Summary of the agent tree for UI display.
#[derive(Debug, Clone)]
pub struct AgentTree {
    pub nodes: Vec<AgentNode>,
    pub tasks: Vec<TaskRun>,
}

/// Manages the lifecycle of sub-agents and their tasks.
///
/// This is a session-scoped registry. It does NOT execute tasks directly
/// — the SessionSupervisor spawns Tokio tasks for each active TaskRun.
/// The manager tracks status and enforces tree invariants (e.g. max concurrent
/// children, parent cancellation cascades).
#[derive(Debug)]
pub struct SubAgentManager {
    nodes: HashMap<AgentId, AgentNode>,
    tasks: HashMap<TaskId, TaskRun>,
    max_children: usize,
}

impl SubAgentManager {
    /// Create a new manager.
    pub fn new(max_children: usize) -> Self {
        Self {
            nodes: HashMap::new(),
            tasks: HashMap::new(),
            max_children,
        }
    }

    /// Register the root (main) agent.
    pub fn register_root(&mut self) -> AgentId {
        let root = AgentNode::new(None, "main");
        let id = root.id;
        self.nodes.insert(id, root);
        id
    }

    /// Spawn a child agent and create its first task.
    ///
    /// Returns an error if the maximum number of children is exceeded.
    pub fn spawn(
        &mut self,
        parent_id: AgentId,
        label: impl Into<String>,
        input: impl Into<String>,
        context_fork: ContextFork,
        budget: TaskBudget,
    ) -> Result<(AgentId, TaskId), String> {
        let active_children = self
            .nodes
            .values()
            .filter(|n| n.parent_id == Some(parent_id) && n.status == AgentStatus::Busy)
            .count();

        if active_children >= self.max_children {
            return Err(format!(
                "max children ({}) exceeded for parent {parent_id}",
                self.max_children
            ));
        }

        let mut node = AgentNode::new(Some(parent_id), label);
        node.status = AgentStatus::Busy;
        let agent_id = node.id;
        self.nodes.insert(agent_id, node);

        let task = TaskRun::new(agent_id, input, context_fork, budget);
        let task_id = task.id;
        self.tasks.insert(task_id, task);

        Ok((agent_id, task_id))
    }

    /// Get a task by id.
    pub fn get_task(&self, task_id: &TaskId) -> Option<&TaskRun> {
        self.tasks.get(task_id)
    }

    /// Get a mutable task by id.
    pub fn get_task_mut(&mut self, task_id: &TaskId) -> Option<&mut TaskRun> {
        self.tasks.get_mut(task_id)
    }

    /// Complete a task and update the agent status.
    pub fn complete_task(&mut self, task_id: &TaskId, result: String, tokens: u64) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.complete(result, tokens);
            if let Some(node) = self.nodes.get_mut(&task.agent_id) {
                node.status = AgentStatus::Idle;
            }
        }
    }

    /// Fail a task and update the agent status.
    pub fn fail_task(&mut self, task_id: &TaskId, error: impl Into<String>) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.fail(error);
            if let Some(node) = self.nodes.get_mut(&task.agent_id) {
                node.status = AgentStatus::Error;
            }
        }
    }

    /// Cancel a task and all its children recursively.
    pub fn cancel(&mut self, agent_id: &AgentId) {
        // Cancel direct tasks.
        for task in self.tasks.values_mut() {
            if task.agent_id == *agent_id && !task.is_terminal() {
                task.cancel();
            }
        }

        // Mark node.
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.status = AgentStatus::Completed;
        }

        // Cancel children recursively.
        let child_ids: Vec<AgentId> = self
            .nodes
            .values()
            .filter(|n| n.parent_id == Some(*agent_id))
            .map(|n| n.id)
            .collect();

        for child_id in child_ids {
            self.cancel(&child_id);
        }
    }

    /// Cancel all pending/running tasks and mark all nodes as completed.
    pub fn cancel_all(&mut self) {
        for task in self.tasks.values_mut() {
            if !task.is_terminal() {
                task.cancel();
            }
        }
        for node in self.nodes.values_mut() {
            if node.status == AgentStatus::Busy {
                node.status = AgentStatus::Completed;
            }
        }
    }

    /// Start a NEW TaskRun on an EXISTING agent node (followup_task path,
    /// Doc 12 §14: followups reuse the same child identity — no spawn).
    ///
    /// Enforces the single-non-terminal-run invariant: an AgentNode may
    /// hold at most one active TaskRun because concurrent runs cannot
    /// safely rewrite the same child Session's Context Projection.
    pub fn start_followup_task(
        &mut self,
        agent_id: AgentId,
        input: impl Into<String>,
        context_fork: ContextFork,
        budget: TaskBudget,
    ) -> Result<TaskId, String> {
        if !self.nodes.contains_key(&agent_id) {
            return Err(format!("agent {agent_id} not found"));
        }
        let busy = self
            .tasks
            .values()
            .any(|t| t.agent_id == agent_id && !t.is_terminal());
        if busy {
            return Err(format!(
                "agent {agent_id} already has a non-terminal TaskRun; followups must queue"
            ));
        }
        let task = TaskRun::new(agent_id, input, context_fork, budget);
        let task_id = task.id;
        self.tasks.insert(task_id, task);
        if let Some(node) = self.nodes.get_mut(&agent_id) {
            node.status = AgentStatus::Busy;
        }
        Ok(task_id)
    }

    /// Register an agent node WITHOUT starting a TaskRun.
    ///
    /// Used by hosts that execute child runs elsewhere (e.g. the
    /// live-loop DelegateTool) but still want the collaboration
    /// protocol's identity/messaging/wait semantics for the child.
    pub fn register_node(
        &mut self,
        parent_id: AgentId,
        label: impl Into<String>,
    ) -> Result<AgentId, String> {
        if !self.nodes.contains_key(&parent_id) {
            return Err(format!("parent {parent_id} not found"));
        }
        let live = self
            .nodes
            .values()
            .filter(|n| n.parent_id == Some(parent_id) && n.status == AgentStatus::Busy)
            .count();
        if live >= self.max_children {
            return Err(format!("max children ({}) reached", self.max_children));
        }
        let node = AgentNode::new(Some(parent_id), label.into());
        let id = node.id;
        self.nodes.insert(id, node);
        Ok(id)
    }

    /// Interrupt ONLY the target agent's non-terminal TaskRuns (Doc 12
    /// §14 `interrupt_agent`: cancel the current run, keep the identity).
    /// Unlike `cancel`, this does not cascade to children and does not
    /// retire the node — it goes back to Idle, ready for followups.
    /// Returns the number of interrupted tasks.
    pub fn interrupt(&mut self, agent_id: &AgentId) -> usize {
        let mut n = 0;
        for task in self.tasks.values_mut() {
            if task.agent_id == *agent_id && !task.is_terminal() {
                task.cancel();
                n += 1;
            }
        }
        if n > 0 {
            if let Some(node) = self.nodes.get_mut(agent_id) {
                node.status = AgentStatus::Idle;
            }
        }
        n
    }

    /// Direct children of an agent (protocol traversal helper).
    pub fn children_of(&self, agent_id: &AgentId) -> Vec<AgentId> {
        self.nodes
            .values()
            .filter(|n| n.parent_id == Some(*agent_id))
            .map(|n| n.id)
            .collect()
    }

    /// The node itself (protocol lookup helper).
    pub fn get_node(&self, agent_id: &AgentId) -> Option<&AgentNode> {
        self.nodes.get(agent_id)
    }

    /// All tasks belonging to one agent, newest first.
    pub fn tasks_of(&self, agent_id: &AgentId) -> Vec<&TaskRun> {
        let mut out: Vec<&TaskRun> = self.tasks.values().filter(|t| t.agent_id == *agent_id).collect();
        out.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        out
    }

    /// Build a tree summary for the UI.
    pub fn tree(&self) -> AgentTree {
        AgentTree {
            nodes: self.nodes.values().cloned().collect(),
            tasks: self.tasks.values().cloned().collect(),
        }
    }

    /// Number of active (non-terminal) tasks.
    pub fn active_task_count(&self) -> usize {
        self.tasks.values().filter(|t| !t.is_terminal()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_complete_child() {
        let mut mgr = SubAgentManager::new(5);
        let root = mgr.register_root();

        let (_agent_id, task_id) = mgr
            .spawn(
                root,
                "test-agent",
                "do something",
                ContextFork::None,
                TaskBudget {
                    max_turns: Some(3),
                    max_duration_secs: None,
                },
            )
            .unwrap();

        assert_eq!(mgr.active_task_count(), 1);
        assert!(mgr.get_task(&task_id).is_some());

        mgr.complete_task(&task_id, "done".into(), 100);
        assert_eq!(mgr.active_task_count(), 0);

        let task = mgr.get_task(&task_id).unwrap();
        assert_eq!(task.result.as_deref(), Some("done"));
        assert!(task.is_terminal());
    }

    #[test]
    fn cancel_cascades_to_children() {
        let mut mgr = SubAgentManager::new(5);
        let root = mgr.register_root();

        let (child_id, _task1) = mgr
            .spawn(
                root,
                "child",
                "task1",
                ContextFork::None,
                TaskBudget {
                    max_turns: Some(1),
                    max_duration_secs: None,
                },
            )
            .unwrap();

        let (_grandchild_id, _task2) = mgr
            .spawn(
                child_id,
                "grandchild",
                "task2",
                ContextFork::None,
                TaskBudget {
                    max_turns: Some(1),
                    max_duration_secs: None,
                },
            )
            .unwrap();

        assert_eq!(mgr.active_task_count(), 2);
        mgr.cancel(&child_id);
        assert_eq!(mgr.active_task_count(), 0);
    }

    #[test]
    fn max_children_limit() {
        let mut mgr = SubAgentManager::new(1);
        let root = mgr.register_root();

        mgr.spawn(
            root,
            "c1",
            "t1",
            ContextFork::None,
            TaskBudget {
                max_turns: None,
                max_duration_secs: None,
            },
        )
        .unwrap();

        let result = mgr.spawn(
            root,
            "c2",
            "t2",
            ContextFork::None,
            TaskBudget {
                max_turns: None,
                max_duration_secs: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn followup_task_enforces_single_non_terminal_run() {
        let mut mgr = SubAgentManager::new(5);
        let root = mgr.register_root();
        let budget = TaskBudget {
            max_turns: Some(3),
            max_duration_secs: None,
        };

        let (agent_id, _task1) = mgr
            .spawn(root, "child", "task1", ContextFork::None, budget.clone())
            .unwrap();

        // Unknown agent is rejected.
        let ghost = AgentId::new();
        assert!(mgr
            .start_followup_task(ghost, "x", ContextFork::None, budget.clone())
            .is_err());

        // A second non-terminal run on the same node must queue, not start.
        let err = mgr
            .start_followup_task(agent_id, "t2", ContextFork::None, budget.clone())
            .unwrap_err();
        assert!(err.contains("non-terminal"));

        // After the current run terminates the same identity accepts a followup.
        let first = mgr.tasks_of(&agent_id)[0].id;
        mgr.complete_task(&first, "done".into(), 10);
        let followup = mgr
            .start_followup_task(agent_id, "t2", ContextFork::None, budget)
            .unwrap();
        assert_eq!(mgr.tasks_of(&agent_id).len(), 2);
        assert_eq!(mgr.get_task(&followup).unwrap().agent_id, agent_id);
        assert_eq!(mgr.get_node(&agent_id).unwrap().status, AgentStatus::Busy);
    }

    #[test]
    fn interrupt_keeps_node_and_does_not_cascade() {
        let mut mgr = SubAgentManager::new(5);
        let root = mgr.register_root();
        let budget = TaskBudget {
            max_turns: Some(1),
            max_duration_secs: None,
        };

        let (child_id, _t1) = mgr
            .spawn(root, "child", "task1", ContextFork::None, budget.clone())
            .unwrap();
        let (grandchild_id, _t2) = mgr
            .spawn(child_id, "grandchild", "task2", ContextFork::None, budget)
            .unwrap();

        assert_eq!(mgr.active_task_count(), 2);
        assert_eq!(mgr.interrupt(&child_id), 1);

        // Only the target's run is cancelled; the grandchild keeps running.
        assert_eq!(mgr.active_task_count(), 1);
        assert!(!mgr.tasks_of(&grandchild_id)[0].is_terminal());

        // The node survives as Idle and stays addressable for followups.
        assert_eq!(mgr.get_node(&child_id).unwrap().status, AgentStatus::Idle);
        assert!(mgr.get_node(&grandchild_id).is_some());
    }
}
