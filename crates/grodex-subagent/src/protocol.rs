//! Parent-child collaboration protocol — the six model-facing tools
//! (Doc 12 §5.2 / §14).
//!
//! | Tool            | Semantics                                              |
//! |-----------------|--------------------------------------------------------|
//! | `send_message`  | queue-only delivery; never starts a Turn               |
//! | `followup_task` | deliver + start/resume: idle → new TaskRun now; busy → |
//! |                 | persist FIFO, consumed after the current run terminates|
//! | `wait_agent`    | bounded wait over DESCENDANT targets only (no cycles)  |
//! | `mailbox_read`  | read-then-ack: bodies enter the transcript only here;  |
//! |                 | the cursor advances only after explicit confirmation   |
//! | `list_agents`   | tree + status + current TaskRun, filtered by path      |
//! | `interrupt_agent` | cancel the target's current TaskRun, KEEP the node   |
//!
//! Hard rules enforced here:
//! - every delivery goes through the `MailboxRouter` event path — no
//!   bypassing into in-memory queues (§14: bypasses lose messages across
//!   unload/reload);
//! - `followup_task`'s trigger is "when idle", never "concurrent": at most
//!   one non-terminal TaskRun per AgentNode, backlog stays FIFO, each
//!   follow-up forms its own TaskRun;
//! - `wait_agent` may only target the caller's own descendants — ancestor,
//!   sibling and lateral waits are rejected, eliminating A-waits-B/B-waits-A
//!   cycles at the protocol level (§14.2 rule 6);
//! - `wait_agent` previews NEVER advance the mailbox cursor; only
//!   `mailbox_read` + confirmation does (§14.1).

use crate::context::ContextFork;
use crate::mailbox::{AgentMessage, MailboxError, MailboxRouter};
use crate::manager::SubAgentManager;
use crate::node::{AgentId, AgentStatus};
use crate::task::{TaskBudget, TaskId, TaskStatus};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Protocol knobs (Doc 12 §14.2).
#[derive(Debug, Clone)]
pub struct ProtocolConfig {
    /// Default `wait_agent` timeout (§14.2 rule 1).
    pub default_wait_timeout: Duration,
    /// Foreground lease — the hard cap on any wait.
    pub foreground_lease: Duration,
    /// Bounded preview length returned by `wait_agent`.
    pub preview_chars: usize,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            default_wait_timeout: Duration::from_secs(30),
            foreground_lease: Duration::from_secs(120),
            preview_chars: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("agent {0} not found")]
    AgentNotFound(AgentId),
    #[error("wait target {target} is not a descendant of caller {caller}: ancestor/sibling/lateral waits are forbidden")]
    NotDescendant { caller: AgentId, target: AgentId },
    #[error("interrupt target {target} is not owned by caller {caller}")]
    NotOwned { caller: AgentId, target: AgentId },
    #[error("mailbox delivery failed: {0}")]
    Mailbox(String),
    #[error("task scheduling refused: {0}")]
    TaskRefused(String),
}

impl From<MailboxError> for ProtocolError {
    fn from(e: MailboxError) -> Self {
        ProtocolError::Mailbox(e.to_string())
    }
}

/// Durable protocol events — delivery and cursor updates live in the same
/// event stream (§14). Persist these into the rollout journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolEvent {
    MessageDelivered { message_id: String, author: AgentId, target: AgentId, trigger_turn: bool },
    FollowupQueued { message_id: String, target: AgentId },
    FollowupTriggered { message_id: String, target: AgentId, task_run_id: TaskId },
    WaitReturned { caller: AgentId, finished: usize, pending: usize, timed_out: bool },
    MailboxRead { agent: AgentId, returned: usize },
    MessagesConfirmed { agent: AgentId, confirmed: usize },
    AgentInterrupted { caller: AgentId, target: AgentId, interrupted_tasks: usize },
}

/// Outcome of `followup_task`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowupOutcome {
    /// Target was idle: message delivered AND a new TaskRun started.
    Triggered { message_id: String, task_run_id: TaskId },
    /// Target was busy: message persisted; a TaskRun starts only after
    /// the current run terminates (FIFO).
    Queued { message_id: String },
}

/// One waited-on target's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitTargetState {
    pub agent_id: AgentId,
    /// Latest TaskRun status, if any run exists.
    pub latest_task: Option<(TaskId, TaskStatus)>,
    /// Whether the target reached a terminal TaskRun.
    pub finished: bool,
    /// Bounded preview of the newest mailbox activity (does NOT advance
    /// the cursor).
    pub preview: Option<String>,
}

/// Bounded `wait_agent` result — a timeout is data, not an error (§14.2
/// rule 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitResult {
    pub finished: Vec<WaitTargetState>,
    pub pending: Vec<WaitTargetState>,
    /// True when pending targets remain and the effective timeout elapsed.
    pub timed_out: bool,
    /// Effective timeout applied (after lease/budget clamping).
    pub effective_timeout: Duration,
    /// Set by the Loop's interjection path when a user steer / parent
    /// cancellation aborted the wait (§14.2 rule 4).
    pub interrupted: bool,
}

/// `mailbox_read` result: bodies + the ids that must be confirmed AFTER
/// the Tool Result is committed (read-then-ack, §14.1).
#[derive(Debug, Clone)]
pub struct MailboxReadResult {
    pub messages: Vec<AgentMessage>,
    /// Message ids to pass to `confirm_consumed` once the Tool Result
    /// has been committed to the transcript.
    pub pending_confirmation: Vec<String>,
    /// Opaque cursor for the next read (diagnostic / replay).
    pub next_cursor: u64,
}

/// One row of `list_agents`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentListing {
    pub agent_id: AgentId,
    /// Label-derived path, e.g. `/main/reviewer/security`.
    pub path: String,
    pub status: AgentStatus,
    /// Current non-terminal TaskRun, or the most recent terminal one.
    pub latest_task_run_id: Option<TaskId>,
    pub latest_task_terminal: Option<bool>,
    pub unread_messages: usize,
}

/// The collaboration protocol facade over the manager + mailbox router.
///
/// Owns both registries; every mutation goes through their event paths.
#[derive(Debug)]
pub struct CollaborationProtocol {
    manager: SubAgentManager,
    router: MailboxRouter,
    config: ProtocolConfig,
    events: Vec<ProtocolEvent>,
}

impl CollaborationProtocol {
    pub fn new(manager: SubAgentManager, router: MailboxRouter, config: ProtocolConfig) -> Self {
        Self { manager, router, config, events: Vec::new() }
    }

    pub fn manager(&self) -> &SubAgentManager { &self.manager }
    pub fn router(&self) -> &MailboxRouter { &self.router }

    /// Mutable access for hosts that register externally-executed children
    /// (node + mailbox) into the protocol tree.
    pub fn manager_mut(&mut self) -> &mut SubAgentManager { &mut self.manager }
    pub fn router_mut(&mut self) -> &mut MailboxRouter { &mut self.router }

    /// Drain the durable protocol events for journal persistence.
    pub fn take_events(&mut self) -> Vec<ProtocolEvent> {
        std::mem::take(&mut self.events)
    }

    // ── Tree helpers ────────────────────────────────────────────────

    /// Label-derived path of an agent (`/main/child/...`).
    pub fn path_of(&self, agent_id: &AgentId) -> Option<String> {
        let mut labels = Vec::new();
        let mut cur = Some(*agent_id);
        while let Some(id) = cur {
            let node = self.manager.get_node(&id)?;
            labels.push(node.label.clone());
            cur = node.parent_id;
        }
        labels.reverse();
        Some(format!("/{}", labels.join("/")))
    }

    /// True iff `node` is a strict descendant of `ancestor`.
    pub fn is_descendant(&self, ancestor: &AgentId, node: &AgentId) -> bool {
        let mut cur = self.manager.get_node(node).and_then(|n| n.parent_id);
        while let Some(id) = cur {
            if id == *ancestor {
                return true;
            }
            cur = self.manager.get_node(&id).and_then(|n| n.parent_id);
        }
        false
    }

    fn latest_task_of(&self, agent_id: &AgentId) -> Option<(TaskId, TaskStatus, bool)> {
        self.manager
            .tasks_of(agent_id)
            .first()
            .map(|t| (t.id, t.status, t.is_terminal()))
    }

    fn has_active_run(&self, agent_id: &AgentId) -> bool {
        self.manager
            .tasks_of(agent_id)
            .first()
            .map(|t| !t.is_terminal())
            .unwrap_or(false)
    }

    fn target_preview(&self, agent_id: &AgentId) -> Option<String> {
        let msgs = self.router.read(agent_id, 1).ok()?;
        let newest = msgs.last()?;
        let p = newest.preview.clone().unwrap_or_default();
        let trunc: String = p.chars().take(self.config.preview_chars).collect();
        Some(trunc)
    }

    // ── 1. send_message ─────────────────────────────────────────────

    /// Queue-only delivery: writes to the target mailbox via the router,
    /// NEVER starts a Turn (§5.2).
    pub fn send_message(
        &mut self,
        author: AgentId,
        target: AgentId,
        message: impl Into<String>,
    ) -> Result<String, ProtocolError> {
        if self.manager.get_node(&target).is_none() {
            return Err(ProtocolError::AgentNotFound(target));
        }
        let msg = AgentMessage::message(author, target, message);
        let id = msg.message_id.to_string();
        self.router.dispatch(msg)?;
        self.events.push(ProtocolEvent::MessageDelivered {
            message_id: id.clone(),
            author,
            target,
            trigger_turn: false,
        });
        Ok(id)
    }

    // ── 2. followup_task ────────────────────────────────────────────

    /// Deliver a follow-up and start/resume the target.
    ///
    /// Idle target → message delivered + new TaskRun created now.
    /// Busy target → message persisted as queued trigger; the Supervisor
    /// consumes it (FIFO) only after the current TaskRun reaches a
    /// terminal state via [`Self::on_task_finished`].
    pub fn followup_task(
        &mut self,
        author: AgentId,
        target: AgentId,
        message: impl Into<String>,
        context_fork: ContextFork,
        budget: TaskBudget,
    ) -> Result<FollowupOutcome, ProtocolError> {
        if self.manager.get_node(&target).is_none() {
            return Err(ProtocolError::AgentNotFound(target));
        }
        let payload: String = message.into();
        let msg = AgentMessage::followup(author, target, payload.clone(), None);
        let id = msg.message_id.to_string();
        self.router.dispatch(msg)?;

        if self.has_active_run(&target) {
            self.events.push(ProtocolEvent::FollowupQueued { message_id: id.clone(), target });
            return Ok(FollowupOutcome::Queued { message_id: id });
        }

        // Idle: consume the just-delivered followup and start the run.
        let followup = self
            .router
            .pop_followup(&target)
            .ok_or_else(|| ProtocolError::Mailbox("queued followup vanished".into()))?;
        let task_id = self
            .manager
            .start_followup_task(target, followup.payload, context_fork, budget)
            .map_err(ProtocolError::TaskRefused)?;
        self.events.push(ProtocolEvent::FollowupTriggered {
            message_id: id.clone(),
            target,
            task_run_id: task_id,
        });
        Ok(FollowupOutcome::Triggered { message_id: id, task_run_id: task_id })
    }

    /// Supervisor hook: after `target`'s TaskRun reached a terminal state,
    /// consume the NEXT queued follow-up (FIFO) and start its TaskRun.
    /// Returns None when no queued trigger remains.
    pub fn on_task_finished(
        &mut self,
        target: AgentId,
        context_fork: ContextFork,
        budget: TaskBudget,
    ) -> Result<Option<FollowupOutcome>, ProtocolError> {
        if self.has_active_run(&target) {
            // Not terminal yet — nothing may be consumed (FIFO order must
            // follow root-rollout commit order).
            return Ok(None);
        }
        let Some(followup) = self.router.pop_followup(&target) else {
            return Ok(None);
        };
        let id = followup.message_id.to_string();
        let task_id = self
            .manager
            .start_followup_task(target, followup.payload, context_fork, budget)
            .map_err(ProtocolError::TaskRefused)?;
        self.events.push(ProtocolEvent::FollowupTriggered {
            message_id: id.clone(),
            target,
            task_run_id: task_id,
        });
        Ok(Some(FollowupOutcome::Triggered { message_id: id, task_run_id: task_id }))
    }

    /// Execution-host hook: mark a TaskRun terminal, then consume the
    /// NEXT queued follow-up for the same agent (FIFO) if any.
    ///
    /// Returns the new `(task_run_id, payload)` so the host (e.g. the
    /// live-loop adapter) can execute it — the protocol never runs
    /// children itself. `Err` is only for unknown task ids; “no queued
    /// follow-up” is `Ok(None)`.
    pub fn finish_task_run(
        &mut self,
        task_id: TaskId,
        outcome: Result<String, String>,
        context_fork: ContextFork,
        budget: TaskBudget,
    ) -> Result<Option<(TaskId, String)>, ProtocolError> {
        let target = self
            .manager
            .get_task(&task_id)
            .map(|t| t.agent_id)
            .ok_or_else(|| ProtocolError::TaskRefused(format!("task {task_id} not found")))?;
        match outcome {
            Ok(result) => self.manager.complete_task(&task_id, result, 0),
            Err(err) => self.manager.fail_task(&task_id, err),
        }
        let Some(followup) = self.router.pop_followup(&target) else {
            return Ok(None);
        };
        let message_id = followup.message_id.to_string();
        let payload = followup.payload.clone();
        let new_id = self
            .manager
            .start_followup_task(target, followup.payload, context_fork, budget)
            .map_err(ProtocolError::TaskRefused)?;
        self.events.push(ProtocolEvent::FollowupTriggered {
            message_id,
            target,
            task_run_id: new_id,
        });
        Ok(Some((new_id, payload)))
    }

    // ── 3. wait_agent ───────────────────────────────────────────────

    /// Bounded wait over descendant targets (§14.2).
    ///
    /// Timeout clamping: requested (else default 30s) → min with the
    /// foreground lease → min with the Turn's remaining wall-time budget.
    /// Timeout yields `timed_out=true` with the finished subset — never a
    /// tool error. Previews never advance the mailbox cursor.
    pub fn wait_agent(
        &mut self,
        caller: AgentId,
        targets: &[AgentId],
        requested_timeout: Option<Duration>,
        remaining_turn_budget: Option<Duration>,
    ) -> Result<WaitResult, ProtocolError> {
        for t in targets {
            if self.manager.get_node(t).is_none() {
                return Err(ProtocolError::AgentNotFound(*t));
            }
            // Own TaskRun targets are covered by task_wait; wait_agent
            // only accepts descendant agents (§14.2 rule 6).
            if !self.is_descendant(&caller, t) {
                return Err(ProtocolError::NotDescendant { caller, target: *t });
            }
        }

        let requested = requested_timeout.unwrap_or(self.config.default_wait_timeout);
        let mut effective = requested.min(self.config.foreground_lease);
        if let Some(budget) = remaining_turn_budget {
            effective = effective.min(budget);
        }

        let mut finished = Vec::new();
        let mut pending = Vec::new();
        for t in targets {
            let latest = self.latest_task_of(t);
            let is_finished = latest.map(|(_, _, terminal)| terminal).unwrap_or(false);
            let state = WaitTargetState {
                agent_id: *t,
                latest_task: latest.map(|(id, st, _)| (id, st)),
                finished: is_finished,
                // Preview only — the cursor must not move (§14.1).
                preview: self.target_preview(t),
            };
            if is_finished { finished.push(state); } else { pending.push(state); }
        }

        let timed_out = !pending.is_empty();
        self.events.push(ProtocolEvent::WaitReturned {
            caller,
            finished: finished.len(),
            pending: pending.len(),
            timed_out,
        });
        Ok(WaitResult { finished, pending, timed_out, effective_timeout: effective, interrupted: false })
    }

    // ── 4. mailbox_read ─────────────────────────────────────────────

    /// Read unconfirmed message bodies into the transcript (read-then-ack,
    /// §14.1). The returned ids must be confirmed via
    /// [`Self::confirm_consumed`] ONLY after the Tool Result itself has
    /// been committed — a crash in between at worst redelivers the same
    /// messages after recovery.
    pub fn mailbox_read(
        &mut self,
        agent: AgentId,
        limit: usize,
    ) -> Result<MailboxReadResult, ProtocolError> {
        if self.manager.get_node(&agent).is_none() {
            return Err(ProtocolError::AgentNotFound(agent));
        }
        let msgs: Vec<AgentMessage> = self
            .router
            .read(&agent, limit)
            .map_err(ProtocolError::Mailbox)?
            .into_iter()
            .cloned()
            .collect();
        let pending_confirmation: Vec<String> =
            msgs.iter().map(|m| m.message_id.to_string()).collect();
        let next_cursor = self.router.unread_count(&agent) as u64;
        self.events.push(ProtocolEvent::MailboxRead { agent, returned: msgs.len() });
        Ok(MailboxReadResult { messages: msgs, pending_confirmation, next_cursor })
    }

    /// Advance the mailbox cursor for messages whose Tool Result has been
    /// committed to the transcript. Returns how many ids were confirmed.
    pub fn confirm_consumed(
        &mut self,
        agent: AgentId,
        message_ids: &[String],
    ) -> Result<usize, ProtocolError> {
        let mut confirmed = 0usize;
        for id_str in message_ids {
            let Some(id) = parse_message_id(id_str) else { continue };
            if self.router.confirm(&agent, id).map_err(ProtocolError::Mailbox)? {
                confirmed += 1;
            }
        }
        self.events.push(ProtocolEvent::MessagesConfirmed { agent, confirmed });
        Ok(confirmed)
    }

    // ── 5. list_agents ──────────────────────────────────────────────

    /// Tree listing with status + latest TaskRun + unread count, filtered
    /// by path prefix (None = whole tree).
    pub fn list_agents(&self, path_prefix: Option<&str>) -> Vec<AgentListing> {
        let tree = self.manager.tree();
        let mut rows: Vec<AgentListing> = tree
            .nodes
            .iter()
            .filter_map(|node| {
                let path = self.path_of(&node.id)?;
                if let Some(prefix) = path_prefix {
                    if !path.starts_with(prefix) {
                        return None;
                    }
                }
                let latest = self.latest_task_of(&node.id);
                Some(AgentListing {
                    agent_id: node.id,
                    path,
                    status: node.status,
                    latest_task_run_id: latest.map(|(id, _, _)| id),
                    latest_task_terminal: latest.map(|(_, _, terminal)| terminal),
                    unread_messages: self.router.unread_count(&node.id),
                })
            })
            .collect();
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        rows
    }

    // ── 6. interrupt_agent ──────────────────────────────────────────

    /// Cancel the target's current TaskRun WITHOUT retiring the node and
    /// WITHOUT cascading to children (§14). Only the caller's own
    /// descendants (or itself) may be interrupted.
    pub fn interrupt_agent(
        &mut self,
        caller: AgentId,
        target: AgentId,
    ) -> Result<usize, ProtocolError> {
        if self.manager.get_node(&target).is_none() {
            return Err(ProtocolError::AgentNotFound(target));
        }
        if caller != target && !self.is_descendant(&caller, &target) {
            return Err(ProtocolError::NotOwned { caller, target });
        }
        let n = self.manager.interrupt(&target);
        self.events.push(ProtocolEvent::AgentInterrupted {
            caller,
            target,
            interrupted_tasks: n,
        });
        Ok(n)
    }
}

fn parse_message_id(s: &str) -> Option<crate::mailbox::MessageId> {
    crate::mailbox::MessageId::from_string(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskBudget;

    fn budget() -> TaskBudget {
        TaskBudget { max_turns: Some(3), max_duration_secs: None }
    }

    /// root → child → grandchild, all mailboxes registered.
    fn setup() -> (CollaborationProtocol, AgentId, AgentId, AgentId) {
        let mut manager = SubAgentManager::new(8);
        let mut router = MailboxRouter::new();
        let root = manager.register_root();
        router.register(root);
        let child = manager
            .spawn(root, "reviewer", "first task", ContextFork::None, budget())
            .map(|(a, _)| a)
            .unwrap();
        router.register(child);
        let grand = manager
            .spawn(child, "security", "deep task", ContextFork::None, budget())
            .map(|(a, _)| a)
            .unwrap();
        router.register(grand);
        (CollaborationProtocol::new(manager, router, ProtocolConfig::default()), root, child, grand)
    }

    #[test]
    fn send_message_is_queue_only_and_router_mediated() {
        let (mut p, root, child, _) = setup();
        let id = p.send_message(root, child, "heads up").unwrap();
        assert!(!id.is_empty());
        // No new TaskRun was created by the message.
        assert_eq!(p.manager().tasks_of(&child).len(), 1); // the spawn task only
        // The router carries the message (no bypass).
        let msgs = p.router().read(&child, 10).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(!msgs[0].trigger_turn);
        // Unknown target refuses.
        assert!(matches!(
            p.send_message(root, AgentId::new(), "x"),
            Err(ProtocolError::AgentNotFound(_))
        ));
    }

    #[test]
    fn followup_idle_target_starts_new_task_run() {
        let (mut p, root, child, _) = setup();
        // Finish the spawn task so the child is idle.
        let spawn_task = p.manager().tasks_of(&child)[0].id;
        p.manager_mut_for_test().complete_task(&spawn_task, "done".into(), 10);

        match p.followup_task(root, child, "next job", ContextFork::None, budget()).unwrap() {
            FollowupOutcome::Triggered { task_run_id, .. } => {
                let t = p.manager().get_task(&task_run_id).unwrap();
                assert_eq!(t.input, "next job");
            }
            other => panic!("expected Triggered, got {other:?}"),
        }
    }

    #[test]
    fn followup_busy_target_queues_fifo_and_consumes_after_terminal() {
        let (mut p, root, child, _) = setup();
        // child is Busy (spawn task pending) → followups queue.
        let q1 = p.followup_task(root, child, "job A", ContextFork::None, budget()).unwrap();
        let q2 = p.followup_task(root, child, "job B", ContextFork::None, budget()).unwrap();
        assert!(matches!(q1, FollowupOutcome::Queued { .. }));
        assert!(matches!(q2, FollowupOutcome::Queued { .. }));
        assert_eq!(p.manager().tasks_of(&child).len(), 1); // still only the spawn run

        // Finish the spawn run → Supervisor consumes the FIRST queued one.
        let spawn_task = p.manager().tasks_of(&child)[0].id;
        p.manager_mut_for_test().complete_task(&spawn_task, "done".into(), 10);
        let next = p.on_task_finished(child, ContextFork::None, budget()).unwrap().unwrap();
        match next {
            FollowupOutcome::Triggered { task_run_id, .. } => {
                assert_eq!(p.manager().get_task(&task_run_id).unwrap().input, "job A");
            }
            _ => panic!(),
        }
        // While job A runs, nothing else may be consumed (FIFO order).
        assert!(p.on_task_finished(child, ContextFork::None, budget()).unwrap().is_none());
        // Finish job A → job B starts as its OWN TaskRun.
        let job_a = p.manager().tasks_of(&child)[0].id;
        p.manager_mut_for_test().complete_task(&job_a, "a".into(), 1);
        let next = p.on_task_finished(child, ContextFork::None, budget()).unwrap().unwrap();
        match next {
            FollowupOutcome::Triggered { task_run_id, .. } => {
                assert_eq!(p.manager().get_task(&task_run_id).unwrap().input, "job B");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn wait_agent_rejects_ancestor_sibling_and_lateral_targets() {
        let (mut p, root, child, grand) = setup();
        // child waiting on root (ancestor) → forbidden.
        assert!(matches!(
            p.wait_agent(child, &[root], None, None),
            Err(ProtocolError::NotDescendant { .. })
        ));
        // grand waiting on child (ancestor) → forbidden.
        assert!(matches!(
            p.wait_agent(grand, &[child], None, None),
            Err(ProtocolError::NotDescendant { .. })
        ));
        // root waiting on child + grand (descendants) → allowed.
        assert!(p.wait_agent(root, &[child, grand], None, None).is_ok());
    }

    #[test]
    fn wait_agent_timeout_is_data_not_error_and_clamps_to_lease() {
        let (mut p, root, child, _) = setup();
        // child has a pending spawn task → wait times out (bounded).
        let r = p
            .wait_agent(root, &[child], Some(Duration::from_secs(5000)), None)
            .unwrap();
        assert!(r.timed_out);
        assert_eq!(r.pending.len(), 1);
        assert!(r.finished.is_empty());
        // Clamped to the foreground lease (120s << 5000s).
        assert_eq!(r.effective_timeout, Duration::from_secs(120));
        // Remaining turn budget clamps further.
        let r = p
            .wait_agent(root, &[child], Some(Duration::from_secs(30)), Some(Duration::from_secs(7)))
            .unwrap();
        assert_eq!(r.effective_timeout, Duration::from_secs(7));

        // Finish the task → the same wait reports it as finished.
        let spawn_task = p.manager().tasks_of(&child)[0].id;
        p.manager_mut_for_test().complete_task(&spawn_task, "done".into(), 10);
        let r = p.wait_agent(root, &[child], None, None).unwrap();
        assert!(!r.timed_out);
        assert_eq!(r.finished.len(), 1);
        assert!(r.finished[0].finished);
    }

    #[test]
    fn wait_preview_does_not_advance_cursor_but_mailbox_read_does_after_confirm() {
        let (mut p, root, child, _) = setup();
        p.send_message(root, child, "body one").unwrap();
        p.send_message(root, child, "body two").unwrap();

        // wait_agent shows a preview without consuming anything.
        let r = p.wait_agent(root, &[child], None, None).unwrap();
        assert!(r.pending[0].preview.is_some());
        assert_eq!(p.router().unread_count(&child), 2);

        // mailbox_read returns both bodies, still unconfirmed.
        let read = p.mailbox_read(child, 10).unwrap();
        assert_eq!(read.messages.len(), 2);
        assert_eq!(p.router().unread_count(&child), 2);

        // Crash semantics: re-read before confirm redelivers the same set.
        let again = p.mailbox_read(child, 10).unwrap();
        assert_eq!(again.messages.len(), 2);

        // Confirm after the Tool Result commits → cursor advances.
        let confirmed = p.confirm_consumed(child, &read.pending_confirmation).unwrap();
        assert_eq!(confirmed, 2);
        assert_eq!(p.router().unread_count(&child), 0);
        let empty = p.mailbox_read(child, 10).unwrap();
        assert!(empty.messages.is_empty());
    }

    #[test]
    fn list_agents_filters_by_path_prefix_with_status() {
        let (mut p, root, child, _grand) = setup();
        p.send_message(root, child, "ping").unwrap();
        let all = p.list_agents(None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].path, "/main");
        assert_eq!(all[1].path, "/main/reviewer");
        assert_eq!(all[2].path, "/main/reviewer/security");

        let subtree = p.list_agents(Some("/main/reviewer"));
        assert_eq!(subtree.len(), 2);
        let reviewer = subtree.iter().find(|r| r.path == "/main/reviewer").unwrap();
        assert_eq!(reviewer.status, AgentStatus::Busy);
        assert!(reviewer.latest_task_run_id.is_some());
        assert_eq!(reviewer.latest_task_terminal, Some(false));
        assert_eq!(reviewer.unread_messages, 1);
    }

    #[test]
    fn interrupt_agent_cancels_run_keeps_node_and_does_not_cascade() {
        let (mut p, root, child, grand) = setup();
        // Root interrupts the child.
        let n = p.interrupt_agent(root, child).unwrap();
        assert_eq!(n, 1);
        // The child's run is cancelled but the node survives (Idle).
        let node = p.manager().get_node(&child).unwrap();
        assert_eq!(node.status, AgentStatus::Idle);
        assert!(p.manager().tasks_of(&child)[0].is_terminal());
        // Grandchild untouched (no cascade).
        assert!(!p.manager().tasks_of(&grand)[0].is_terminal());
        // The child can take a followup right away.
        match p.followup_task(root, child, "resume work", ContextFork::None, budget()).unwrap() {
            FollowupOutcome::Triggered { .. } => {}
            other => panic!("expected Triggered after interrupt, got {other:?}"),
        }
        // Non-owner cannot interrupt.
        assert!(matches!(
            p.interrupt_agent(grand, root),
            Err(ProtocolError::NotOwned { .. })
        ));
    }

    #[test]
    fn every_operation_emits_a_durable_protocol_event() {
        let (mut p, root, child, _) = setup();
        p.send_message(root, child, "m").unwrap();
        p.wait_agent(root, &[child], None, None).unwrap();
        p.mailbox_read(child, 5).unwrap();
        p.interrupt_agent(root, child).unwrap();
        let events = p.take_events();
        use ProtocolEvent::*;
        assert!(events.iter().any(|e| matches!(e, MessageDelivered { .. })));
        assert!(events.iter().any(|e| matches!(e, WaitReturned { .. })));
        assert!(events.iter().any(|e| matches!(e, MailboxRead { .. })));
        assert!(events.iter().any(|e| matches!(e, AgentInterrupted { .. })));
        // Drain is destructive: second take is empty.
        assert!(p.take_events().is_empty());
    }
}

// Test-only mutable access to the manager (keeps the facade honest in
// production: the Supervisor drives task completion).
#[cfg(test)]
impl CollaborationProtocol {
    fn manager_mut_for_test(&mut self) -> &mut SubAgentManager {
        &mut self.manager
    }
}
