//! RolloutWriter — atomic seq counter + formatted journal writes.
//!
//! Wraps `Arc<dyn RolloutStore>` and manages a monotonic `AtomicU64` seq.
//! Every state transition in the loop writes through this single writer,
//! ensuring sequential, gap-free journal entries.
//!
//! This is the SINGLE source of truth for journal seq numbers: both the
//! `SessionSupervisor` (user input / turn completion / runtime state) and
//! the `TurnCoordinator` (model output / tool calls / tool results /
//! compaction) MUST write through the same shared `RolloutWriter` instance.
//! Writing `RolloutEvent` directly with a hand-rolled `seq` (or `seq: 0`)
//! is forbidden — it breaks the gap-free invariant the `SessionReducer`
//! relies on for crash recovery.
//!
//! Commit fence: the `write_*` helpers return the assigned seq on success
//! and propagate store errors. Callers that gate a state transition on
//! persistence (notably invariant #7 — a Tool Result must be durable
//! before the next sampling step) MUST check the `Result` and abort the
//! turn on `Err` rather than continuing.

use grodex_core::error::GrodexError;
use grodex_core::id::{SessionId, StepGeneration, StepId, TurnId};
use grodex_rollout::event::{RolloutEvent, RolloutEventType, SensitivityLevel};
use grodex_rollout::store::RolloutStore;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Manages journal writes with a single monotonic seq counter shared by
/// every part of the loop. Cloneable — the inner state is behind `Arc`.
#[derive(Clone)]
pub struct RolloutWriter {
    store: Arc<dyn RolloutStore>,
    session_id: SessionId,
    seq: Arc<AtomicU64>,
}

impl RolloutWriter {
    pub fn new(store: Arc<dyn RolloutStore>, session_id: SessionId) -> Self {
        Self {
            store,
            session_id,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Pre-seed the seq counter from an existing journal (crash recovery).
    /// After a replay the writer must continue from the next seq so newly
    /// appended events don't collide with replayed ones.
    pub fn resume_from(&self, next_seq: u64) {
        self.seq.store(next_seq, Ordering::SeqCst);
    }

    /// Number of events written through this writer (for assertions/debug).
    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    /// Store access (recovery code reads via the same handle).
    pub fn store(&self) -> &Arc<dyn RolloutStore> {
        &self.store
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Append one event, returning the assigned seq on success.
    ///
    /// `generation` carries the Step generation active when the event was
    /// produced (invariant #14: late events are validated against it by the
    /// reducer). Passing `None` is only valid for session-lifecycle events
    /// (RuntimeStateChanged) that are not tied to a Step.
    async fn write(
        &self,
        event_type: RolloutEventType,
        turn_id: Option<TurnId>,
        step_id: Option<StepId>,
        generation: Option<StepGeneration>,
        payload: serde_json::Value,
    ) -> Result<u64, GrodexError> {
        let event = RolloutEvent {
            schema_version: 2,
            seq: self.next_seq(),
            session_id: self.session_id,
            turn_id,
            step_id,
            generation,
            timestamp: chrono::Utc::now(),
            event_type,
            payload,
            sensitivity: SensitivityLevel::Normal,
        };
        self.store.append_event(event).await
    }

    pub async fn write_state(&self, state: &str) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::RuntimeStateChanged,
            None,
            None,
            None,
            serde_json::json!({"state": state}),
        )
        .await
    }

    /// Persist context items restored from a prior session journal during
    /// `resume`. Written at the start of a new session so a second crash
    /// does not lose the recovered history. The reducer replays this event
    /// to rebuild the transcript without needing the original journal.
    pub async fn write_context_restored(
        &self,
        items: &[grodex_core::context::ContextItem],
    ) -> Result<u64, GrodexError> {
        let payload = serde_json::json!({
            "items": items,
        });
        self.write(
            RolloutEventType::ContextRestored,
            None,
            None,
            None,
            payload,
        )
        .await
    }

    pub async fn write_user_input(&self, turn_id: TurnId, text: &str) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::UserInputAccepted,
            Some(turn_id),
            None,
            None,
            serde_json::json!({"text": text}),
        )
        .await
    }

    pub async fn write_step_started(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::ToolCallPrepared, // reuse as step boundary
            Some(turn_id),
            Some(step_id),
            Some(generation),
            serde_json::json!({"phase": "step_started"}),
        )
        .await
    }

    pub async fn write_model_output(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        assistant_text: &str,
        tool_calls: &[serde_json::Value],
        reasoning: Option<&str>,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "assistant_text": assistant_text,
            "tool_calls": tool_calls,
        });
        if let Some(r) = reasoning {
            payload["reasoning"] = serde_json::json!(r);
        }
        self.write(
            RolloutEventType::ModelItemProduced,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            payload,
        )
        .await
    }

    pub async fn write_tool_started(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        call_id: &str,
        name: &str,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::ToolExecutionStarted,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            serde_json::json!({"call_id": call_id, "name": name}),
        )
        .await
    }

    pub async fn write_tool_finished(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        call_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::ToolResultCommitted,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            serde_json::json!({"call_id": call_id, "content": content, "is_error": is_error}),
        )
        .await
    }

    /// Record that a tool execution finished (success or error), distinct
    /// from `ToolResultCommitted` which fires when the result is durable.
    /// `ToolExecutionFinished` fires the instant the tool returns, before
    /// the result is persisted to the journal.
    pub async fn write_tool_execution_finished(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        call_id: &str,
        is_error: bool,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::ToolExecutionFinished,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            serde_json::json!({"call_id": call_id, "is_error": is_error}),
        )
        .await
    }

    pub async fn write_turn_completed(&self, turn_id: TurnId) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::TurnCompleted,
            Some(turn_id),
            None,
            None,
            serde_json::json!({}),
        )
        .await
    }

    pub async fn write_compaction(
        &self,
        turn_id: Option<TurnId>,
        items: &[grodex_core::context::ContextItem],
    ) -> Result<u64, GrodexError> {
        let items_json: Vec<serde_json::Value> = items
            .iter()
            .filter_map(|i| serde_json::to_value(i).ok())
            .collect();
        self.write(
            RolloutEventType::CompactionCommitted,
            turn_id,
            None,
            None,
            serde_json::json!({"items": items_json}),
        )
        .await
    }

    /// Persist a model-route observability event (Design Doc 14 §13.4).
    ///
    /// Route events are emitted at failover decision points: candidate
    /// selected, succeeded, failed, breaker opened, route exhausted. They
    /// are NOT gated by invariant #7 (no state transition depends on them)
    /// but are written for observability and post-hoc debugging.
    pub async fn write_route_event(
        &self,
        turn_id: Option<TurnId>,
        event_kind: &str,
        candidate_id: &str,
        details: serde_json::Value,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::ModelRouteEvent,
            turn_id,
            None,
            None,
            serde_json::json!({
                "event_kind": event_kind,
                "candidate_id": candidate_id,
                "details": details,
            }),
        )
        .await
    }
}
