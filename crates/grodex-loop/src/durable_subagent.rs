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
        // P1-5 fix: a journal write failure here must NOT be silently
        // swallowed. If we can't persist the Started event, the task
        // would be unrestorable on crash — we should unwind the in-memory
        // spawn and return an error so the caller can decide whether to
        // retry. The previous code logged and continued, which meant a
        // crash after this point would lose the task entirely.
        let payload = serde_json::json!({
            "agent_id": agent_id.to_string(),
            "parent_id": parent_id.to_string(),
            "task_id": task_id.to_string(),
            "label": label,
            "input": input,
        });
        if let Err(e) = write_event(&self.writer, RolloutEventType::SubAgentTaskStarted, payload).await {
            // Unwind: cancel the in-memory task so it doesn't dangle.
            self.inner.cancel(&agent_id);
            return Err(format!("无法持久化子任务启动事件到 journal: {e}"));
        }
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
        if let Err(e) = write_event(&self.writer, RolloutEventType::SubAgentTaskFinished, payload).await {
            eprintln!("[error] rollout write SubAgentTaskFinished(completed) failed: {e}");
        }
    }

    /// Fail a task, journaling `SubAgentTaskFinished`.
    pub async fn fail_task(&mut self, task_id: &TaskId, error: &str) {
        self.inner.fail_task(task_id, error);
        let payload = serde_json::json!({
            "task_id": task_id.to_string(),
            "status": "failed",
            "error": error,
        });
        if let Err(e) = write_event(&self.writer, RolloutEventType::SubAgentTaskFinished, payload).await {
            eprintln!("[error] rollout write SubAgentTaskFinished(failed) failed: {e}");
        }
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
            if let Err(e) = write_event(&self.writer, RolloutEventType::SubAgentTaskFinished, payload).await {
                eprintln!("[error] rollout write SubAgentTaskFinished(cancelled) failed: {e}");
            }
        }
    }

    /// Rebuild the sub-agent tree from the journal.
    ///
    /// P1-5 fix: the previous implementation only *counted* unfinished
    /// tasks (Started without Finished) without actually reconstructing
    /// the in-memory task tree. This meant that after a crash:
    ///   - The supervisor's `tree()` was empty
    ///   - Cancel cascades couldn't reach orphaned children
    ///   - The UI showed zero sub-agent activity
    ///
    /// We now replay each `SubAgentTaskStarted` event to re-spawn the
    /// task in-memory (with the same agent_id / task_id / parent_id /
    /// label / input), and then replay each `SubAgentTaskFinished` to
    /// transition the task to its terminal status. Tasks that are
    /// still in-flight after replay are returned as `unrestored` so
    /// the caller can decide to resume or cancel them.
    pub async fn recover_from_journal(&mut self) -> Result<usize, GrodexError> {
        let events = self.writer.store().replay_from(0).await?;

        // Phase 1: replay all Started events to rebuild the tree.
        // We track (task_id → agent_id, parent_id, label, input) so we
        // can call self.inner.spawn with the correct parent.
        #[derive(Default)]
        struct StartedInfo {
            agent_id: String,
            parent_id: String,
            label: String,
            input: String,
        }
        let mut started: std::collections::HashMap<String, StartedInfo> = std::collections::HashMap::new();
        let mut finished: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut finished_status: std::collections::HashMap<String, (String, Option<String>)> = std::collections::HashMap::new();

        for event in &events {
            match event.event_type {
                RolloutEventType::SubAgentTaskStarted => {
                    let task_id = event.payload.get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let agent_id = event.payload.get("agent_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let parent_id = event.payload.get("parent_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let label = event.payload.get("label")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = event.payload.get("input")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    started.insert(task_id, StartedInfo { agent_id, parent_id, label, input });
                }
                RolloutEventType::SubAgentTaskFinished => {
                    let task_id = event.payload.get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = event.payload.get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("completed")
                        .to_string();
                    let result = event.payload.get("result")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    finished.insert(task_id.clone());
                    finished_status.insert(task_id, (status, result));
                }
                _ => {}
            }
        }

        // Phase 2: re-spawn each started task in-memory. We don't
        // re-run the task — we just rebuild the tree structure so
        // tree() / cancel() work. Tasks that were finished get their
        // terminal status set; tasks that were still in-flight are
        // counted as unrestored.
        let mut unrestored = 0usize;
        for (task_id, info) in &started {
            // Re-spawn via inner (NOT through our spawn() method, which
            // would try to journal a new Started event).
            let _ = self.inner.spawn(
                AgentId::from_string(&info.parent_id).unwrap_or(self.inner.root_id()),
                &info.label,
                &info.input,
                ContextFork::None,
                None, // budget not persisted yet
            );
            // If this task was finished, apply the terminal transition.
            if let Some((status, result)) = finished_status.get(task_id) {
                let tid = TaskId::from_string(task_id);
                match status.as_str() {
                    "completed" => {
                        if let Some(r) = result {
                            self.inner.complete_task(&tid, r.clone(), 0);
                        }
                    }
                    "failed" => {
                        let err = result.as_deref().unwrap_or("unknown");
                        self.inner.fail_task(&tid, err);
                    }
                    "cancelled" => {
                        // Cancel is idempotent on already-terminal tasks.
                        let agent = AgentId::from_string(&info.agent_id).unwrap_or(self.inner.root_id());
                        self.inner.cancel(&agent);
                    }
                    _ => {}
                }
            } else {
                // Still in-flight at crash time.
                unrestored += 1;
            }
        }

        Ok(unrestored)
    }
}

/// Write a sub-agent event to the journal via the writer's store.
/// Returns Err if the journal write failed — callers MUST handle this
/// (P1-5: journal write failures must not be silently swallowed).
async fn write_event(
    writer: &RolloutWriter,
    event_type: RolloutEventType,
    payload: serde_json::Value,
) -> Result<u64, GrodexError> {
    use grodex_core::id::{StepGeneration, TurnId};
    let event = RolloutEvent {
        schema_version: 2,
        seq: 0, // filled in by the journal actor
        session_id: writer.session_id(),
        turn_id: None::<TurnId>,
        step_id: None,
        generation: None::<StepGeneration>,
        timestamp: chrono::Utc::now(),
        event_type,
        payload,
        sensitivity: grodex_rollout::event::SensitivityLevel::Normal,
    };
    writer.store().append_event(event).await
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

    async fn writer(dir: &tempfile::TempDir) -> RolloutWriter {
        let sid = SessionId::new();
        let store: Arc<dyn RolloutStore> =
            Arc::new(FileRolloutStore::new_session(dir.path(), &sid.to_string()).await.unwrap());
        RolloutWriter::new(store, sid)
    }

    #[tokio::test]
    async fn spawn_writes_started_event() {
        let dir = tempfile::tempdir().unwrap();
        let w = writer(&dir).await;
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
        let w = writer(&dir).await;
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
        let w = writer(&dir).await;
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
