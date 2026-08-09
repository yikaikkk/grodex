//! Runtime journal handle abstraction — decouples grodex-subagent from
//! grodex-loop's durable event journal via a trait. grodex-loop later
//! implements this trait to emit SubAgentTaskStarted/Finished events.

use crate::node::AgentId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Metadata carried in both Started and Finished journal events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskJournalMeta {
    pub task_id: String,
    pub agent_id: AgentId,
    pub parent_agent_id: Option<AgentId>,
    pub authority_ceiling: u8,
    pub delegated_from: Option<AgentId>,
    pub started_at_ms: u64,
}

/// Result summary for the Finished journal event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResultSummary {
    pub exit_code: i32,
    pub output_head_tail_b64: Option<String>,
    pub error_reason: Option<String>,
    pub finished_at_ms: u64,
}

/// Abstract handle for emitting runtime journal events.
///
/// grodex-subagent never depends on grodex-loop directly; instead the
/// higher-level loop crate implements this trait and injects it via
/// `Scheduler::new`. If None/NoopJournalHandle is supplied, scheduling
/// still works — journal writes are simply skipped (fail-soft).
#[async_trait]
pub trait RuntimeJournalHandle: Send + Sync {
    async fn emit_subagent_task_started(&self, meta: TaskJournalMeta) -> anyhow::Result<()>;
    async fn emit_subagent_task_finished(
        &self,
        meta: TaskJournalMeta,
        result: TaskResultSummary,
    ) -> anyhow::Result<()>;
}

/// Default no-op implementation — used when no journal is attached.
pub struct NoopJournalHandle;

#[async_trait]
impl RuntimeJournalHandle for NoopJournalHandle {
    async fn emit_subagent_task_started(&self, _meta: TaskJournalMeta) -> anyhow::Result<()> {
        Ok(())
    }
    async fn emit_subagent_task_finished(
        &self,
        _meta: TaskJournalMeta,
        _result: TaskResultSummary,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}
