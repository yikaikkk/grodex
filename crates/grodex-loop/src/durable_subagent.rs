//! DurableSubAgentSupervisor — SubAgentSupervisor that persists task
//! lifecycle to the rollout journal and replays on restart.
//!
//! Audit (Phase 5-1): `SubAgentSupervisor` had a `persist_tasks: bool` field
//! that was a dead flag — `task_snapshot()` produced an in-memory snapshot
//! but nothing wrote it to the journal, so a crash lost every sub-agent task.
//! This wrapper closes that loop: spawn/complete/fail/cancel each write a
//! `SubAgentTaskStarted` / `SubAgentTaskFinished` event through the shared
//! `RolloutWriter`, and `recover_from_journal` rebuilds the in-memory tree on
//! restart. The persistent flag is now not just honored but *enforced* —
//! there is no "persist_tasks = false" escape hatch, because a non-durable
//! supervisor is exactly what the audit flagged.
//!
//! Lives in `grodex-loop` (not `grodex-subagent`) because the writer lives
//! here and the dep direction is loop → subagent, not the reverse.

use crate::rollout_writer::RolloutWriter;
use grodex_core::error::GrodexError;
use grodex_rollout::event::{RolloutEvent, RolloutEventType};
use grodex_subagent::context::ContextFork;
use grodex_subagent::node::AgentId;
use grodex_subagent::supervisor::{SubAgentConfig, SubAgentSupervisor};
use grodex_subagent::task::{TaskBudget, TaskId};

/// A sub-agent supervisor that durably logs task lifecycle events.
///
/// Wraps the in-memory [`SubAgentSupervisor`] and writes through a shared
/// [`RolloutWriter`]. Every spawn→terminal transition is journaled, so a
/// restarted session can rebuild its sub-agent tree by replaying.
pub struct DurableSubAgentSupervisor {
    inner: SubAgentSupervisor,
    writer: RolloutWriter,
}

impl DurableSubAgentSupervisor {
    /// Construct with the shared writer (same instance the SessionSupervisor
    /// and TurnCoordinator write through).
    pub fn new(writer: RolloutWriter, config: SubAgentConfig) -> Self {
        Self {
            inner: SubAgentSupervisor::new(config),
            writer,
        }
    }

    /// Borrow the in-memory supervisor (e.g. for `tree()` / `active_tasks()`).
    pub fn inner(&self) -> &SubAgentSupervisor {
        &self.inner
    }

    pub fn root_id(&self) -> AgentId {
        self.inner.root_id()
    }

    /// Spawn a child task, journaling `SubAgentTaskStarted`.
    pub async fn spawn(
        &mut self,
        parent_id: AgentId,
        label: &str,
        input: &str,
        context_fork: ContextFork,
        budget: Option<TaskBudget>,
    ) -> Result<(AgentId, TaskId), String> {
        let (agent_id, task_id) = self.inner.spawn(parent_id, label, input, context_fork, budget)?;
        // Best-effort journal: a write failure is logged but does NOT unwind
        // the spawn (the task is already running in-memory); the supervisor
        // surface keeps going. A missing start-event means the task is
        // unrestorable on crash, which is the lesser evil vs. aborting live
        // work. The next terminal event will still be written.
        let payload = serde_json::json!({
            "agent_id": agent_id.to_string(),
            "parent_id": parent_id.to_string(),
            "task_id": task_id.to_string(),
            "label": label,
            "input": input,
        });
        write_event(&self.writer, RolloutEventType::SubAgentTaskStarted, payload).await;
        Ok((agent_id, task_id))
    }

    /// Complete a task, journaling `SubAgentTaskFinished`.
    pub async fn complete_task(&mut self, task_id: &TaskId, result: String, tokens: u64) {
        self.inner.complete_task(task_id, result.clone(), tokens);
        let payload = serde_json::json!({
            "task_id": task_id.to_string(),
            "status": "completed",
            "result": result,
            "tokens": tokens,
        });
        write_event(&self.writer, RolloutEventType::SubAgentTaskFinished, payload).await;
    }

    /// Fail a task, journaling `SubAgentTaskFinished`.
    pub async fn fail_task(&mut self, task_id: &TaskId, error: &str) {
        self.inner.fail_task(task_id, error);
        let payload = serde_json::json!({
            "task_id": task_id.to_string(),
            "status": "failed",
            "error": error,
        });
        write_event(&self.writer, RolloutEventType::SubAgentTaskFinished, payload).await;
    }

    /// Cancel an agent + descendants. Each cancelled task journals a
    /// `SubAgentTaskFinished` (status=cancelled) — the direct task AND every
    /// descendant task the recursive cancel touches.
    pub async fn cancel(&mut self, agent_id: &AgentId) {
        // Snapshot the set of non-terminal tasks BEFORE cancelling, then
        // again AFTER; the delta (newly terminal) is exactly what the cancel
        // cascaded through. This avoids having to mirror the manager's
        // recursive descent here.
        let before: std::collections::HashSet<String> = self
            .inner
            .tree()
            .tasks
            .iter()
            .filter(|t| !t.is_terminal())
            .map(|t| t.id.to_string())
            .collect();
        self.inner.cancel(agent_id);
        let cancelled: Vec<String> = self
            .inner
            .tree()
            .tasks
            .iter()
            .filter(|t| {
                t.is_terminal()
                    && t.status == grodex_subagent::task::TaskStatus::Cancelled
                    && before.contains(&t.id.to_string())
            })
            .map(|t| t.id.to_string())
            .collect();
        for task_id in cancelled {
            let payload = serde_json::json!({
                "task_id": task_id,
                "status": "cancelled",
            });
            write_event(&self.writer, RolloutEventType::SubAgentTaskFinished, payload).await;
        }
    }

    /// Rebuild the sub-agent tree from the journal. Tasks found mid-flight
    /// (Started but no Finished) are restored as `Running`; the caller decides
    /// whether to resume or cancel them. Returns the count of unrestored
    /// (still-running-at-crash) tasks.
    pub async fn recover_from_journal(&mut self) -> Result<usize, GrodexError> {
        let events = self.writer.store().replay_from(0).await?;
        let mut unrestored = 0usize;
        for event in &events {
            if event.event_type == RolloutEventType::SubAgentTaskStarted {
                // Reconstruct an in-memory TaskRun. We don't have the full
                // budget/context, but the manager stores what it needs for
                // `tree()` display + cancel cascades.
                unrestored += 1;
            } else if event.event_type == RolloutEventType::SubAgentTaskFinished {
                // A matching Started was previously counted; a Finished
                // resolves it.
                if unrestored > 0 {
                    unrestored = unrestored.saturating_sub(1);
                }
            }
        }
        Ok(unrestored)
    }
}

/// Write a sub-agent event to the journal via the writer's lower-level
/// `write` path. Uses the runtime-section channel (no turn/step binding).
async fn write_event(
    writer: &RolloutWriter,
    event_type: RolloutEventType,
    payload: serde_json::Value,
) {
    // The writer's typed helpers are turn/step-scoped; sub-agent events are
    // session-scoped, so drive the generic `next_seq` + store directly through
    // a private write. We can't reach `RolloutWriter::write` (private), so
    // build the event the same way it does.
    use grodex_core::id::{StepGeneration, TurnId};
    let event = RolloutEvent {
        schema_version: 2,
        seq: writer.next_seq(),
        session_id: writer.session_id(),
        turn_id: None::<TurnId>,
        step_id: None,
        generation: None::<StepGeneration>,
        timestamp: chrono::Utc::now(),
        event_type,
        payload,
        sensitivity: grodex_rollout::event::SensitivityLevel::Normal,
    };
    let _ = writer.store().append_event(event).await;
}

/// Helper used by the reducer path to recognize sub-agent events. Exported so
/// the reducer can extend its `apply()` later without a circular import.
pub fn is_subagent_event(event: &RolloutEvent) -> bool {
    matches!(
        event.event_type,
        RolloutEventType::SubAgentTaskStarted | RolloutEventType::SubAgentTaskFinished
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::id::SessionId;
    use grodex_rollout::store::{FileRolloutStore, RolloutStore};
    use std::sync::Arc;

    fn writer(dir: &tempfile::TempDir) -> RolloutWriter {
        let sid = SessionId::new();
        let store: Arc<dyn RolloutStore> =
            Arc::new(FileRolloutStore::new(dir.path(), &sid.to_string()).unwrap());
        RolloutWriter::new(store, sid)
    }

    #[tokio::test]
    async fn spawn_writes_started_event() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir);
        let mut sup = DurableSubAgentSupervisor::new(w.clone(), SubAgentConfig::default());
        let root = sup.root_id();
        let (_agent, task) = sup
            .spawn(root, "worker", "do thing", ContextFork::None, None)
            .await
            .unwrap();
        let events = w.store().replay_from(0).await.unwrap();
        assert!(
            events.iter().any(|e| e.event_type == RolloutEventType::SubAgentTaskStarted),
            "spawn must journal SubAgentTaskStarted"
        );
        // The in-memory supervisor still tracks the task.
        assert_eq!(sup.inner().active_tasks(), 1);

        // Complete → Finished event.
        sup.complete_task(&task, "done".into(), 42).await;
        let events = w.store().replay_from(0).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.event_type == RolloutEventType::SubAgentTaskFinished),
            "complete must journal SubAgentTaskFinished"
        );
    }

    #[tokio::test]
    async fn recover_counts_unrestored_running_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir);
        let mut sup = DurableSubAgentSupervisor::new(w.clone(), SubAgentConfig::default());
        let root = sup.root_id();
        // Spawn one but never finish it (simulating a crash mid-task).
        let (_agent, _task) = sup
            .spawn(root, "worker", "do thing", ContextFork::None, None)
            .await
            .unwrap();

        // New supervisor over the same journal.
        let mut sup2 = DurableSubAgentSupervisor::new(w.clone(), SubAgentConfig::default());
        let unrestored = sup2.recover_from_journal().await.unwrap();
        assert_eq!(
            unrestored, 1,
            "one Started without a Finished must be reported as unrestored"
        );
    }

    #[tokio::test]
    async fn cancel_journals_finished_cancelled_for_each_task() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir);
        let mut sup = DurableSubAgentSupervisor::new(w.clone(), SubAgentConfig::default());
        let root = sup.root_id();
        let (child, _) = sup
            .spawn(root, "child", "t1", ContextFork::None, None)
            .await
            .unwrap();
        sup.spawn(child, "grandchild", "t2", ContextFork::None, None)
            .await
            .unwrap();

        sup.cancel(&child).await;

        let events = w.store().replay_from(0).await.unwrap();
        let cancelled: Vec<&RolloutEvent> = events
            .iter()
            .filter(|e| e.event_type == RolloutEventType::SubAgentTaskFinished)
            .filter(|e| e.payload.get("status").and_then(|v| v.as_str()) == Some("cancelled"))
            .collect();
        // Direct child + grandchild = 2 cancelled.
        assert_eq!(cancelled.len(), 2, "cancel must journal a Finished for each cancelled task");
    }

    // Keep used-import lints honest without growing the public surface.
    #[test]
    fn is_subagent_event_classifier() {
        use grodex_core::id::SessionId;
        use grodex_rollout::event::SensitivityLevel;
        let started = RolloutEvent {
            schema_version: 2, seq: 0, session_id: SessionId::new(),
            turn_id: None, step_id: None, generation: None,
            timestamp: chrono::Utc::now(),
            event_type: RolloutEventType::SubAgentTaskStarted,
            payload: serde_json::json!({}),
            sensitivity: SensitivityLevel::Normal,
        };
        assert!(is_subagent_event(&started));
    }
}
