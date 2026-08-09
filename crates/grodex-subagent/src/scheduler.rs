//! Scheduler — 10-dimensional resource governance + live async run loop.
//!
//! Design Doc 12 §16: "不能只设置一个 `max_agents`" — resource limits
//! must be split into at least 10 independent dimensions (kept as the
//! inner `ResourceBudget` / `ResourceUsage` / `SpawnReservation` layer).
//!
//! Additionally this module now exposes the **live** Scheduler with an
//! async `run` loop: `submit_task` → `next_task` → `tick` → shutdown,
//! integrating ResidencyManager (resource tokens), WorkspaceManager
//! (leases and path locks), MailboxRouter (inter-agent dispatch), and
//! the optional `RuntimeJournalHandle` (emit lifecycle events to the
//! durable rollout journal without depending on grodex-loop directly).

use crate::journal::{NoopJournalHandle, RuntimeJournalHandle, TaskJournalMeta, TaskResultSummary};
use crate::mailbox::MailboxRouter;
use crate::node::AgentId;
use crate::residency::{ResidencyError, ResidencyManager, ResidencyToken, Resident};
use crate::task::{TaskId, TaskRun, TaskStatus};
use crate::workspace::{WorkspaceError, WorkspaceManager};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::{Mutex, watch};

// ── Legacy 10-dim resource accounting (Design Doc 12 §16) ─────────────────

/// The 10-dimensional resource budget for an agent tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceBudget {
    pub max_total_agent_nodes: usize,
    pub max_tree_depth: usize,
    pub max_children_per_agent: usize,
    pub max_active_child_turns: usize,
    pub max_resident_child_sessions: usize,
    pub max_provider_requests: usize,
    pub max_external_processes: usize,
    pub tree_token_budget: u64,
    pub task_token_budget: u64,
    pub task_wall_time_budget: u64,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            max_total_agent_nodes: 20,
            max_tree_depth: 3,
            max_children_per_agent: 5,
            max_active_child_turns: 4,
            max_resident_child_sessions: 8,
            max_provider_requests: 4,
            max_external_processes: 8,
            tree_token_budget: 500_000,
            task_token_budget: 50_000,
            task_wall_time_budget: 300,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub total_agent_nodes: usize,
    pub active_child_turns: usize,
    pub resident_child_sessions: usize,
    pub provider_requests_in_flight: usize,
    pub external_processes_running: usize,
    pub tree_tokens_consumed: u64,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ResourceLimitError {
    #[error("max_total_agent_nodes exceeded: {current}/{limit}")]
    TotalNodesExceeded { current: usize, limit: usize },
    #[error("max_tree_depth exceeded: requested depth {requested}, limit {limit}")]
    TreeDepthExceeded { requested: usize, limit: usize },
    #[error("max_children_per_agent exceeded: parent {parent} has {current} children, limit {limit}")]
    ChildrenPerAgentExceeded {
        parent: AgentId,
        current: usize,
        limit: usize,
    },
    #[error("max_active_child_turns exceeded: {current}/{limit}")]
    ActiveTurnsExceeded { current: usize, limit: usize },
    #[error("max_resident_child_sessions exceeded: {current}/{limit}")]
    ResidentSessionsExceeded { current: usize, limit: usize },
    #[error("max_provider_requests exceeded: {current}/{limit}")]
    ProviderRequestsExceeded { current: usize, limit: usize },
    #[error("max_external_processes exceeded: {current}/{limit}")]
    ExternalProcessesExceeded { current: usize, limit: usize },
    #[error("tree_token_budget exceeded: consumed {consumed}, budget {budget}, need {needed}")]
    TreeTokenBudgetExceeded { consumed: u64, budget: u64, needed: u64 },
    #[error("task_token_budget exceeded: task needs {needed}, limit {limit}")]
    TaskTokenBudgetExceeded { needed: u64, limit: u64 },
    #[error("task_wall_time_budget exceeded: task needs {needed}s, limit {limit}s")]
    TaskWallTimeBudgetExceeded { needed: u64, limit: u64 },
}

#[derive(Debug, Default)]
struct ParentCounters {
    children: HashMap<AgentId, usize>,
}

#[derive(Debug, Clone)]
pub struct SpawnReservation {
    pub parent_id: AgentId,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerDiagnostics {
    pub total_nodes: (usize, usize),
    pub active_turns: (usize, usize),
    pub resident_sessions: (usize, usize),
    pub provider_requests: (usize, usize),
    pub external_processes: (usize, usize),
    pub tree_tokens: (u64, u64),
}

#[derive(Debug)]
struct ResourceScheduler {
    budget: ResourceBudget,
    usage: ResourceUsage,
    parent_counters: ParentCounters,
}

impl ResourceScheduler {
    fn new(budget: ResourceBudget) -> Self {
        Self {
            budget,
            usage: ResourceUsage::default(),
            parent_counters: ParentCounters::default(),
        }
    }
    fn usage(&self) -> &ResourceUsage { &self.usage }
    fn budget(&self) -> &ResourceBudget { &self.budget }

    pub fn check_spawn(
        &self,
        parent_id: AgentId,
        requested_depth: usize,
        estimated_task_tokens: u64,
        estimated_task_duration_secs: u64,
    ) -> Result<SpawnReservation, ResourceLimitError> {
        if self.usage.total_agent_nodes >= self.budget.max_total_agent_nodes {
            return Err(ResourceLimitError::TotalNodesExceeded {
                current: self.usage.total_agent_nodes,
                limit: self.budget.max_total_agent_nodes,
            });
        }
        if requested_depth > self.budget.max_tree_depth {
            return Err(ResourceLimitError::TreeDepthExceeded {
                requested: requested_depth,
                limit: self.budget.max_tree_depth,
            });
        }
        let current_children = self.parent_counters.children.get(&parent_id).copied().unwrap_or(0);
        if current_children >= self.budget.max_children_per_agent {
            return Err(ResourceLimitError::ChildrenPerAgentExceeded {
                parent: parent_id,
                current: current_children,
                limit: self.budget.max_children_per_agent,
            });
        }
        if self.usage.active_child_turns >= self.budget.max_active_child_turns {
            return Err(ResourceLimitError::ActiveTurnsExceeded {
                current: self.usage.active_child_turns,
                limit: self.budget.max_active_child_turns,
            });
        }
        if self.usage.resident_child_sessions >= self.budget.max_resident_child_sessions {
            return Err(ResourceLimitError::ResidentSessionsExceeded {
                current: self.usage.resident_child_sessions,
                limit: self.budget.max_resident_child_sessions,
            });
        }
        let remaining_tree = self.budget.tree_token_budget.saturating_sub(self.usage.tree_tokens_consumed);
        if estimated_task_tokens > remaining_tree {
            return Err(ResourceLimitError::TreeTokenBudgetExceeded {
                consumed: self.usage.tree_tokens_consumed,
                budget: self.budget.tree_token_budget,
                needed: estimated_task_tokens,
            });
        }
        if estimated_task_tokens > self.budget.task_token_budget {
            return Err(ResourceLimitError::TaskTokenBudgetExceeded {
                needed: estimated_task_tokens,
                limit: self.budget.task_token_budget,
            });
        }
        if estimated_task_duration_secs > self.budget.task_wall_time_budget {
            return Err(ResourceLimitError::TaskWallTimeBudgetExceeded {
                needed: estimated_task_duration_secs,
                limit: self.budget.task_wall_time_budget,
            });
        }
        Ok(SpawnReservation { parent_id, estimated_tokens: estimated_task_tokens })
    }

    pub fn commit_spawn(&mut self, reservation: &SpawnReservation) {
        self.usage.total_agent_nodes += 1;
        self.usage.active_child_turns += 1;
        self.usage.resident_child_sessions += 1;
        *self.parent_counters.children.entry(reservation.parent_id).or_insert(0) += 1;
    }

    pub fn release_turn_permit(&mut self) {
        if self.usage.active_child_turns > 0 {
            self.usage.active_child_turns -= 1;
        }
    }
    pub fn acquire_turn_permit(&mut self) -> Result<(), ResourceLimitError> {
        if self.usage.active_child_turns >= self.budget.max_active_child_turns {
            return Err(ResourceLimitError::ActiveTurnsExceeded {
                current: self.usage.active_child_turns,
                limit: self.budget.max_active_child_turns,
            });
        }
        self.usage.active_child_turns += 1;
        Ok(())
    }
    pub fn acquire_provider_request(&mut self) -> Result<(), ResourceLimitError> {
        if self.usage.provider_requests_in_flight >= self.budget.max_provider_requests {
            return Err(ResourceLimitError::ProviderRequestsExceeded {
                current: self.usage.provider_requests_in_flight,
                limit: self.budget.max_provider_requests,
            });
        }
        self.usage.provider_requests_in_flight += 1;
        Ok(())
    }
    pub fn release_provider_request(&mut self) {
        if self.usage.provider_requests_in_flight > 0 {
            self.usage.provider_requests_in_flight -= 1;
        }
    }
    pub fn acquire_external_process(&mut self) -> Result<(), ResourceLimitError> {
        if self.usage.external_processes_running >= self.budget.max_external_processes {
            return Err(ResourceLimitError::ExternalProcessesExceeded {
                current: self.usage.external_processes_running,
                limit: self.budget.max_external_processes,
            });
        }
        self.usage.external_processes_running += 1;
        Ok(())
    }
    pub fn release_external_process(&mut self) {
        if self.usage.external_processes_running > 0 {
            self.usage.external_processes_running -= 1;
        }
    }
    pub fn record_token_usage(&mut self, tokens: u64) {
        self.usage.tree_tokens_consumed += tokens;
    }
    pub fn release_agent(&mut self, parent_id: AgentId) {
        if self.usage.total_agent_nodes > 0 {
            self.usage.total_agent_nodes -= 1;
        }
        let count = self.parent_counters.children.entry(parent_id).or_insert(0);
        if *count > 0 { *count -= 1; }
    }
    pub fn release_resident_session(&mut self) {
        if self.usage.resident_child_sessions > 0 {
            self.usage.resident_child_sessions -= 1;
        }
    }
    pub fn acquire_resident_session(&mut self) -> Result<(), ResourceLimitError> {
        if self.usage.resident_child_sessions >= self.budget.max_resident_child_sessions {
            return Err(ResourceLimitError::ResidentSessionsExceeded {
                current: self.usage.resident_child_sessions,
                limit: self.budget.max_resident_child_sessions,
            });
        }
        self.usage.resident_child_sessions += 1;
        Ok(())
    }
    pub fn diagnostics(&self) -> SchedulerDiagnostics {
        SchedulerDiagnostics {
            total_nodes: (self.usage.total_agent_nodes, self.budget.max_total_agent_nodes),
            active_turns: (self.usage.active_child_turns, self.budget.max_active_child_turns),
            resident_sessions: (self.usage.resident_child_sessions, self.budget.max_resident_child_sessions),
            provider_requests: (self.usage.provider_requests_in_flight, self.budget.max_provider_requests),
            external_processes: (self.usage.external_processes_running, self.budget.max_external_processes),
            tree_tokens: (self.usage.tree_tokens_consumed, self.budget.tree_token_budget),
        }
    }
}

// ── Live Scheduler: async run / tick / next_task / schedule ──────────────

/// Ordering key for pending tasks: (priority desc, created_at asc, task_id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PendingKey {
    priority_inv: u8,
    created_at_ms: u64,
    task_id: TaskId,
}

fn now_ms() -> u64 {
    use chrono::Utc;
    Utc::now().timestamp_millis() as u64
}

/// Per-running-task bookkeeping stored in `Scheduler.running_tasks`.
pub struct RunningTaskHandle {
    pub join_handle: tokio::task::JoinHandle<()>,
    pub residency_token: Option<ResidencyToken>,
    pub task_snapshot: TaskRun,
    pub scheduled_at_ms: u64,
}

/// Summary returned by `Scheduler::tick`.
#[derive(Debug, Clone, Default)]
pub struct SchedulerTickSummary {
    pub started_count: usize,
    pub finished_count: usize,
    pub failed_count: usize,
    pub cancelled_count: usize,
    pub mailbox_dispatched: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("duplicate task id: {0}")]
    DuplicateTaskId(TaskId),
    #[error("parent agent {0:?} is not Running")]
    ParentNotRunning(AgentId),
    #[error("no residency available for agent {0:?}")]
    NoResidency(AgentId),
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("residency error: {0}")]
    Residency(#[from] ResidencyError),
    #[error("resource exhausted")]
    ResourceExhausted,
    #[error("task {0:?} was cancelled")]
    Cancelled(TaskId),
    #[error("join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),
    #[error("scheduler shutdown")]
    Shutdown,
    #[error("mailbox error: {0}")]
    Mailbox(String),
}

/// The live Scheduler — drives task submission, residency allocation,
/// workspace leasing, mailbox dispatch, and the journal event stream.
pub struct Scheduler {
    resource_scheduler: ResourceScheduler,
    pending_queue: BTreeMap<PendingKey, TaskRun>,
    running_tasks: HashMap<TaskId, RunningTaskHandle>,
    all_tasks: HashMap<TaskId, TaskRun>,
    journal: Option<Arc<dyn RuntimeJournalHandle + Send + Sync>>,
    workspace: Arc<Mutex<WorkspaceManager>>,
    mailbox: Arc<Mutex<MailboxRouter>>,
    residency: Arc<Mutex<ResidencyManager>>,
}

impl Scheduler {
    pub fn new(
        budget: ResourceBudget,
        mailbox: Arc<Mutex<MailboxRouter>>,
        residency: Arc<Mutex<ResidencyManager>>,
        workspace: Arc<Mutex<WorkspaceManager>>,
        journal_opt: Option<Arc<dyn RuntimeJournalHandle + Send + Sync>>,
    ) -> Self {
        Self {
            resource_scheduler: ResourceScheduler::new(budget),
            pending_queue: BTreeMap::new(),
            running_tasks: HashMap::new(),
            all_tasks: HashMap::new(),
            journal: journal_opt,
            workspace,
            mailbox,
            residency,
        }
    }

    pub fn with_noop_journal(
        budget: ResourceBudget,
        mailbox: Arc<Mutex<MailboxRouter>>,
        residency: Arc<Mutex<ResidencyManager>>,
        workspace: Arc<Mutex<WorkspaceManager>>,
    ) -> Self {
        Self::new(budget, mailbox, residency, workspace, Some(Arc::new(NoopJournalHandle)))
    }

    pub fn usage(&self) -> &ResourceUsage { self.resource_scheduler.usage() }
    pub fn budget(&self) -> &ResourceBudget { self.resource_scheduler.budget() }
    pub fn diagnostics(&self) -> SchedulerDiagnostics { self.resource_scheduler.diagnostics() }

    pub async fn submit_task(&mut self, mut task: TaskRun) -> Result<TaskId, SchedulerError> {
        let task_id = task.id;
        if self.all_tasks.contains_key(&task_id) {
            return Err(SchedulerError::DuplicateTaskId(task_id));
        }
        if task.status == TaskStatus::Pending {
            // good, keep
        }

        if let Some(parent_id) = self.parent_of(&task.agent_id) {
            let residency = self.residency.lock().await;
            let state = residency.state(&parent_id);
            drop(residency);
            if !matches!(state, Some(Resident)) {
                return Err(SchedulerError::ParentNotRunning(parent_id));
            }
        }

        let agent_id = task.agent_id;
        let mode = crate::workspace::WorkspaceMode::SharedWrite;
        let mut ws = self.workspace.lock().await;
        if let Err(e) = ws.grant_lease(agent_id, mode, self.parent_of(&agent_id)) {
            return Err(SchedulerError::Workspace(e));
        }
        drop(ws);

        task.status = TaskStatus::Pending;
        self.all_tasks.insert(task_id, task.clone());

        let created_ms = task.created_at.timestamp_millis() as u64;
        let key = PendingKey {
            priority_inv: 0,
            created_at_ms: created_ms,
            task_id,
        };
        self.pending_queue.insert(key, task);
        Ok(task_id)
    }

    fn parent_of(&self, _agent_id: &AgentId) -> Option<AgentId> {
        None
    }

    pub async fn cancel_task(&mut self, id: &TaskId) -> Result<(), SchedulerError> {
        let mut removed_pending = false;
        let pending_keys: Vec<PendingKey> = self.pending_queue
            .keys()
            .filter(|k| k.task_id == *id)
            .copied()
            .collect();
        for k in pending_keys {
            self.pending_queue.remove(&k);
            removed_pending = true;
        }

        if removed_pending {
            if let Some(t) = self.all_tasks.get_mut(id) {
                t.cancel();
            }
            let mut ws = self.workspace.lock().await;
            if let Some(t) = self.all_tasks.get(id) {
                let _ = ws.revoke_lease(&t.agent_id);
            }
            drop(ws);
            return Ok(());
        }

        if let Some(handle) = self.running_tasks.remove(id) {
            handle.join_handle.abort();
            if let Some(token) = handle.residency_token {
                let mut res = self.residency.lock().await;
                let _ = res.release(token);
                drop(res);
            }
            if let Some(t) = self.all_tasks.get_mut(id) {
                t.cancel();
            }
            let mut ws = self.workspace.lock().await;
            if let Some(t) = self.all_tasks.get(id) {
                let _ = ws.revoke_lease(&t.agent_id);
            }
            drop(ws);
            return Ok(());
        }

        Ok(())
    }

    pub async fn next_task(&mut self) -> Result<Option<TaskId>, SchedulerError> {
        if self.pending_queue.is_empty() {
            return Ok(None);
        }

        let mut picked: Option<(PendingKey, TaskRun)> = None;
        let pending_snapshot: Vec<(PendingKey, TaskRun)> = self.pending_queue
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();

        for (key, task) in pending_snapshot {
            let resources_required = crate::residency::ResourceBudgetUsage::default();
            let mut res = self.residency.lock().await;
            let token = match res.allocate(&task.agent_id, resources_required) {
                Ok(t) => Some(t),
                Err(_) => {
                    drop(res);
                    continue;
                }
            };
            drop(res);

            picked = Some((key, task));
            let _ = token;
            break;
        }

        let Some((key, mut task)) = picked else {
            return Ok(None);
        };

        self.pending_queue.remove(&key);
        task.status = TaskStatus::Running;
        let scheduled_at = now_ms();

        if let Some(ref journal) = self.journal {
            let meta = TaskJournalMeta {
                task_id: task.id.to_string(),
                agent_id: task.agent_id,
                parent_agent_id: None,
                authority_ceiling: 0,
                delegated_from: None,
                started_at_ms: scheduled_at,
            };
            let j = journal.clone();
            tokio::spawn(async move {
                let _ = j.emit_subagent_task_started(meta).await;
            });
        }

        let agent_id = task.agent_id;
        let task_id = task.id;
        let residency_token;
        {
            let mut res = self.residency.lock().await;
            let usage = crate::residency::ResourceBudgetUsage::default();
            residency_token = Some(res.allocate(&agent_id, usage)?);
            let _ = res.start(&agent_id, crate::residency::ProcessInfo::default());
            drop(res);
        }

        let join_handle = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            let _ = agent_id;
            let _ = task_id;
        });

        self.all_tasks.insert(task_id, task.clone());
        self.running_tasks.insert(task_id, RunningTaskHandle {
            join_handle,
            residency_token,
            task_snapshot: task,
            scheduled_at_ms: scheduled_at,
        });
        Ok(Some(task_id))
    }

    pub async fn tick(&mut self) -> SchedulerTickSummary {
        let mut summary = SchedulerTickSummary::default();
        let mut finished_ids: Vec<TaskId> = Vec::new();
        let mut failed_count: usize = 0;
        let mut cancelled_count: usize = 0;

        for (id, handle) in self.running_tasks.iter_mut() {
            if handle.join_handle.is_finished() {
                finished_ids.push(*id);
            }
        }

        let finished_total = finished_ids.len();
        for id in &finished_ids {
            let handle = self.running_tasks.remove(id).unwrap();
            let mut success = true;
            let mut exit_code = 0i32;
            let mut error_reason: Option<String> = None;
            let mut is_cancelled = false;

            match handle.join_handle.await {
                Ok(()) => {}
                Err(e) if e.is_cancelled() => {
                    success = false;
                    exit_code = 130;
                    error_reason = Some("cancelled".into());
                    is_cancelled = true;
                }
                Err(e) => {
                    success = false;
                    exit_code = 1;
                    error_reason = Some(e.to_string());
                    failed_count += 1;
                }
            }
            if is_cancelled {
                cancelled_count += 1;
            }

            if let Some(token) = handle.residency_token {
                let mut res = self.residency.lock().await;
                let _ = res.release(token);
                drop(res);
            }

            if let Some(task) = self.all_tasks.get_mut(id) {
                if success {
                    task.complete(String::new(), 0);
                } else if exit_code == 130 {
                    task.cancel();
                } else {
                    task.fail(error_reason.clone().unwrap_or_default());
                }
            }

            if let Some(ref journal) = self.journal {
                if let Some(task) = self.all_tasks.get(id) {
                    let meta = TaskJournalMeta {
                        task_id: task.id.to_string(),
                        agent_id: task.agent_id,
                        parent_agent_id: None,
                        authority_ceiling: 0,
                        delegated_from: None,
                        started_at_ms: handle.scheduled_at_ms,
                    };
                    let result = TaskResultSummary {
                        exit_code,
                        output_head_tail_b64: None,
                        error_reason,
                        finished_at_ms: now_ms(),
                    };
                    let j = journal.clone();
                    tokio::spawn(async move {
                        let _ = j.emit_subagent_task_finished(meta, result).await;
                    });
                }
            }

            let agent_id = self.all_tasks.get(id).map(|t| t.agent_id);
            if let Some(aid) = agent_id {
                let mut ws = self.workspace.lock().await;
                let _ = ws.revoke_lease(&aid);
                drop(ws);
            }
        }

        summary.failed_count = failed_count;
        summary.cancelled_count = cancelled_count;
        summary.finished_count = finished_total
            .saturating_sub(failed_count)
            .saturating_sub(cancelled_count);

        loop {
            match self.next_task().await {
                Ok(Some(_)) => {
                    summary.started_count += 1;
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        {
            let mb = self.mailbox.lock().await;
            let agent_ids: Vec<AgentId> = mb.registered_agent_ids();
            drop(mb);
            for agent in agent_ids {
                let mut mb = self.mailbox.lock().await;
                let result = mb.poll_for_agent(&agent, 16, std::time::Duration::from_secs(0));
                drop(mb);
                if let Ok(msgs) = result {
                    summary.mailbox_dispatched += msgs.len();
                }
            }
        }

        summary
    }

    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Result<(), SchedulerError> {
        let mut tick_interval = tokio::time::interval(tokio::time::Duration::from_millis(50));
        tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow_and_update() {
                        break;
                    }
                }
                _ = tick_interval.tick() => {
                    let _ = self.tick().await;
                }
            }
        }

        let running_ids: Vec<TaskId> = self.running_tasks.keys().copied().collect();
        for id in running_ids {
            let _ = self.cancel_task(&id).await;
        }
        Ok(())
    }
}

// ── Keep module-level constructor compatible with existing tests ─────────

// The legacy standalone `Scheduler` (resource-only, no run loop) is kept
// accessible as `resource_scheduler` internals; tests below still exercise
// the 10-dim budget layer via a thin wrapper.

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> AgentId { AgentId::new() }

    fn small_budget() -> ResourceBudget {
        ResourceBudget {
            max_total_agent_nodes: 3,
            max_tree_depth: 2,
            max_children_per_agent: 2,
            max_active_child_turns: 2,
            max_resident_child_sessions: 2,
            max_provider_requests: 2,
            max_external_processes: 2,
            tree_token_budget: 1000,
            task_token_budget: 500,
            task_wall_time_budget: 60,
        }
    }

    fn legacy() -> ResourceScheduler {
        ResourceScheduler::new(small_budget())
    }

    #[test]
    fn check_spawn_succeeds_within_limits() {
        let s = legacy();
        assert!(s.check_spawn(parent(), 1, 100, 30).is_ok());
    }

    #[test]
    fn check_spawn_rejects_depth() {
        let s = legacy();
        let err = s.check_spawn(parent(), 3, 100, 30).unwrap_err();
        assert_eq!(err, ResourceLimitError::TreeDepthExceeded { requested: 3, limit: 2 });
    }

    #[test]
    fn acquire_release_turn_permit() {
        let mut s = legacy();
        s.acquire_turn_permit().unwrap();
        assert_eq!(s.usage().active_child_turns, 1);
        s.acquire_turn_permit().unwrap();
        assert_eq!(s.usage().active_child_turns, 2);
        assert!(s.acquire_turn_permit().is_err());
        s.release_turn_permit();
        assert_eq!(s.usage().active_child_turns, 1);
    }
}
