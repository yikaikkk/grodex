//! SessionReducer — replays rollout events to rebuild session state.
//!
//! The Reducer is the inverse of the journal writer: it folds a sequence
//! of `RolloutEvent` items back into a coherent session context. This
//! enables crash recovery, session replay, and audit verification.
//!
//! Key invariants enforced by the reducer:
//!   - Monotonic event seq numbers
//!   - Valid state transitions
//!   - Tool-call/result pairing integrity
//!   - Generation monotonicity

use grodex_core::context::ContextItem;
use grodex_core::id::{SessionId, StepGeneration, TurnId};
use grodex_core::state::{SessionState, TurnState};
use grodex_rollout::event::{RolloutEvent, RolloutEventType};
use std::collections::HashMap;

/// Errors that can occur during replay.
#[derive(Debug, thiserror::Error)]
pub enum ReducerError {
    #[error("duplicate event seq: {0}")]
    DuplicateSeq(u64),
    #[error("missing event seq: expected {expected}, got {got}")]
    SeqGap { expected: u64, got: u64 },
    #[error("invalid state transition: {0}")]
    InvalidTransition(String),
    #[error("orphaned tool result: call_id={0}")]
    OrphanedToolResult(String),
    #[error("generation regression: {prev} -> {next}")]
    GenerationRegression { prev: u64, next: u64 },
    #[error("session mismatch: expected {expected}, got {got}")]
    SessionMismatch { expected: String, got: String },
}

/// Rebuilds session state from a sequence of rollout events.
///
/// Usage:
/// ```ignore
/// let events = store.replay_from(0)?;
/// let reducer = SessionReducer::new(session_id);
/// reducer.apply_all(&events)?;
/// let context = reducer.into_context();
/// ```
#[derive(Debug)]
pub struct SessionReducer {
    session_id: SessionId,
    context: Vec<ContextItem>,
    state: SessionState,
    turn_state: Option<(TurnId, TurnState)>,
    current_generation: StepGeneration,
    last_seq: u64,
    /// Active tool calls awaiting results.
    pending_tool_calls: HashMap<String, ContextItem>,
}

impl SessionReducer {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            context: Vec::new(),
            state: SessionState::Initializing,
            turn_state: None,
            current_generation: StepGeneration::initial(),
            last_seq: 0,
            pending_tool_calls: HashMap::new(),
        }
    }

    /// Apply a single event.
    pub fn apply(&mut self, event: &RolloutEvent) -> Result<(), ReducerError> {
        // Validate session id.
        if event.session_id != self.session_id {
            return Err(ReducerError::SessionMismatch {
                expected: self.session_id.to_string(),
                got: event.session_id.to_string(),
            });
        }

        // Validate seq monotonicity.
        if event.seq != self.last_seq {
            return Err(ReducerError::SeqGap {
                expected: self.last_seq,
                got: event.seq,
            });
        }
        self.last_seq += 1;

        // Validate generation monotonicity.
        if let Some(ref g) = event.generation {
            if g.as_u64() < self.current_generation.as_u64() {
                return Err(ReducerError::GenerationRegression {
                    prev: self.current_generation.as_u64(),
                    next: g.as_u64(),
                });
            }
            self.current_generation = *g;
        }

        // Dispatch by event type.
        match event.event_type {
            RolloutEventType::RuntimeStateChanged => {
                if let Some(state_str) = event.payload.get("state").and_then(|v| v.as_str()) {
                    self.state = match state_str {
                        "idle" => SessionState::Idle,
                        "running" => SessionState::Running,
                        "shutting_down" => SessionState::ShuttingDown,
                        "closed" => SessionState::Closed,
                        _ => SessionState::Initializing,
                    };
                }
            }
            RolloutEventType::UserInputAccepted => {
                if let Some(text) = event.payload.get("text").and_then(|v| v.as_str()) {
                    self.context.push(ContextItem::User {
                        content: text.to_string(),
                        message_id: None,
                    });
                }
            }
            RolloutEventType::ModelItemProduced => {
                // Reasoning summary (DeepSeek/Qwen thinking mode) — pushed
                // before the assistant text so the ChatCompletions projection
                // can merge it into the assistant message's reasoning_content.
                if let Some(reasoning) = event.payload.get("reasoning").and_then(|v| v.as_str()) {
                    if !reasoning.is_empty() {
                        self.context.push(ContextItem::ReasoningSummary {
                            content: reasoning.to_string(),
                        });
                    }
                }
                // Assistant text.
                if let Some(text) = event
                    .payload
                    .get("assistant_text")
                    .and_then(|v| v.as_str())
                {
                    if !text.is_empty() {
                        self.context.push(ContextItem::Assistant {
                            content: text.to_string(),
                        });
                    }
                }
                // Tool calls.
                if let Some(tool_calls) = event.payload.get("tool_calls").and_then(|v| v.as_array())
                {
                    for tc in tool_calls {
                        if let (Some(name), Some(args)) = (
                            tc.get("name").and_then(|v| v.as_str()),
                            tc.get("arguments"),
                        ) {
                            // Use the call_id from the payload if present, otherwise generate one.
                            let call_id = tc
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .and_then(|s| grodex_core::id::ToolCallId::from_string(s).ok())
                                .unwrap_or_else(grodex_core::id::ToolCallId::new);
                            let item = ContextItem::ToolCall {
                                call_id,
                                name: name.to_string(),
                                arguments: args.clone(),
                            };
                            self.pending_tool_calls
                                .insert(call_id.to_string(), item.clone());
                            self.context.push(item);
                        }
                    }
                }
            }
            RolloutEventType::ToolResultCommitted => {
                if let (Some(call_id_str), Some(content)) = (
                    event.payload.get("call_id").and_then(|v| v.as_str()),
                    event.payload.get("content").and_then(|v| v.as_str()),
                ) {
                    let is_error = event
                        .payload
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let call_id =
                        grodex_core::id::ToolCallId::from_string(call_id_str).unwrap_or_default();
                    self.pending_tool_calls.remove(call_id_str);
                    self.context.push(ContextItem::ToolResult {
                        call_id,
                        content: content.to_string(),
                        is_error,
                    });
                }
            }
            RolloutEventType::CompactionCommitted => {
                // Compaction replaces context with summary + recent items.
                // The payload carries the reconstructed items.
                if let Some(items) = event.payload.get("items").and_then(|v| v.as_array()) {
                    let mut new_context = Vec::new();
                    for item in items {
                        if let Ok(ci) = serde_json::from_value::<ContextItem>(item.clone()) {
                            new_context.push(ci);
                        }
                    }
                    if !new_context.is_empty() {
                        self.context = new_context;
                        self.pending_tool_calls.clear();
                    }
                }
            }
            RolloutEventType::TurnCompleted => {
                self.turn_state = None;
                // Validate no orphaned tool results.
                if !self.pending_tool_calls.is_empty() {
                    let orphaned: Vec<String> =
                        self.pending_tool_calls.keys().cloned().collect();
                    return Err(ReducerError::OrphanedToolResult(orphaned.join(", ")));
                }
            }
            RolloutEventType::ContextRestored => {
                // Context items restored from a prior session during resume.
                // Deserialize and append each item so the transcript is
                // rebuilt without needing the original journal.
                if let Some(items) = event.payload.get("items").and_then(|v| v.as_array()) {
                    for item_val in items {
                        if let Ok(item) = serde_json::from_value::<ContextItem>(item_val.clone()) {
                            self.context.push(item);
                        }
                    }
                }
            }
            _ => {
                // Other event types are logged but don't change context state.
            }
        }

        Ok(())
    }

    /// Apply a batch of events.
    ///
    /// Tolerance: if this reducer is brand-new (`last_seq == 0`, never
    /// applied an event) and the first event in the batch has `seq > 0`,
    /// we skip the missing prefix by re-baselining `last_seq` to the
    /// first event's seq. This is required because some resume paths may
    /// only preserve events starting from position 1 (e.g. missing the
    /// initial `RuntimeStateChanged` at seq=0) but still expect the
    /// state machine to continue strictly monotonically from the first
    /// preserved seq. Without this leniency, users hit the cryptic
    /// `"missing event seq: expected 0, got 1"` after `/resume`.
    pub fn apply_all(&mut self, events: &[RolloutEvent]) -> Result<(), ReducerError> {
        if self.last_seq == 0 {
            if let Some(first) = events.first() {
                if first.seq > 0 {
                    self.last_seq = first.seq;
                }
            }
        }
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }

    /// Apply events from a sequence number, assuming prior events already applied.
    pub fn apply_from(
        &mut self,
        events: &[RolloutEvent],
        from_seq: u64,
    ) -> Result<(), ReducerError> {
        self.last_seq = from_seq;
        self.apply_all(events)
    }

    /// Consume the reducer and return the rebuilt context.
    pub fn into_context(self) -> Vec<ContextItem> {
        self.context
    }

    /// Get the current context without consuming.
    pub fn context(&self) -> &[ContextItem] {
        &self.context
    }

    /// Get the current session state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Get the current generation.
    pub fn generation(&self) -> StepGeneration {
        self.current_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use grodex_rollout::event::{RolloutEvent, RolloutEventType, SensitivityLevel};

    fn make_event(
        session_id: SessionId,
        seq: u64,
        event_type: RolloutEventType,
        payload: serde_json::Value,
    ) -> RolloutEvent {
        RolloutEvent {
            schema_version: 2,
            seq,
            session_id,
            turn_id: None,
            step_id: None,
            generation: None,
            timestamp: Utc::now(),
            event_type,
            payload,
            sensitivity: SensitivityLevel::Normal,
        }
    }

    #[test]
    fn replay_user_and_assistant() {
        let sid = SessionId::new();
        let events = vec![
            make_event(sid, 0, RolloutEventType::RuntimeStateChanged, serde_json::json!({"state": "idle"})),
            make_event(sid, 1, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "hello"})),
            make_event(sid, 2, RolloutEventType::ModelItemProduced, serde_json::json!({"assistant_text": "hi there"})),
            make_event(sid, 3, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];

        let mut reducer = SessionReducer::new(sid);
        reducer.apply_all(&events).unwrap();
        let ctx = reducer.into_context();

        assert_eq!(ctx.len(), 2);
        assert!(matches!(ctx[0], ContextItem::User { .. }));
        assert!(matches!(ctx[1], ContextItem::Assistant { .. }));
    }

    #[test]
    fn replay_tool_call_roundtrip() {
        let sid = SessionId::new();
        let cid = grodex_core::id::ToolCallId::new();
        let events = vec![
            make_event(sid, 0, RolloutEventType::RuntimeStateChanged, serde_json::json!({"state": "idle"})),
            make_event(sid, 1, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "read /tmp"})),
            make_event(sid, 2, RolloutEventType::ModelItemProduced, serde_json::json!({
                "assistant_text": "",
                "tool_calls": [{"call_id": cid.to_string(), "name": "read_file", "arguments": {"path": "/tmp/test.txt"}}]
            })),
            make_event(sid, 3, RolloutEventType::ToolResultCommitted, serde_json::json!({
                "call_id": cid.to_string(),
                "content": "file contents",
                "is_error": false
            })),
            make_event(sid, 4, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];

        let mut reducer = SessionReducer::new(sid);
        reducer.apply_all(&events).unwrap();
        let ctx = reducer.into_context();

        assert_eq!(ctx.len(), 3); // User + ToolCall + ToolResult
        assert!(matches!(ctx[1], ContextItem::ToolCall { .. }));
        assert!(matches!(ctx[2], ContextItem::ToolResult { .. }));
    }

    #[test]
    fn seq_gap_detected() {
        let sid = SessionId::new();
        // apply_all adapts to the first event's seq (for resume), so a
        // gap is only detected BETWEEN events — not at the first event.
        let events = vec![
            make_event(sid, 0, RolloutEventType::RuntimeStateChanged, serde_json::json!({"state": "idle"})),
            make_event(sid, 5, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "gap"})),
        ];

        let mut reducer = SessionReducer::new(sid);
        let err = reducer.apply_all(&events).unwrap_err();
        assert!(matches!(err, ReducerError::SeqGap { expected: 1, got: 5 }));
    }

    #[test]
    fn orphaned_tool_call_detected() {
        let sid = SessionId::new();
        let events = vec![
            make_event(sid, 0, RolloutEventType::RuntimeStateChanged, serde_json::json!({"state": "idle"})),
            make_event(sid, 1, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "test"})),
            make_event(sid, 2, RolloutEventType::ModelItemProduced, serde_json::json!({
                "assistant_text": "",
                "tool_calls": [{"name": "read_file", "arguments": {"path": "/x"}}]
            })),
            // Missing ToolResultCommitted — orphaned tool call
            make_event(sid, 3, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];

        let mut reducer = SessionReducer::new(sid);
        let err = reducer.apply_all(&events).unwrap_err();
        assert!(matches!(err, ReducerError::OrphanedToolResult(_)));
    }
}
