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
    #[error("journal read failed: {0}")]
    JournalRead(String),
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
    /// Tool calls that reached ToolExecutionFinished (content captured)
    /// but whose ToolResultCommitted was never written. On finish(),
    /// these get a synthetic ToolResult from the captured content
    /// instead of the generic "[interrupted]" message.
    finished_not_committed: HashMap<String, (String, bool)>,
    /// Resume mode: repair orphaned tool calls instead of hard-failing.
    ///
    /// A user interrupt aborts the turn task mid-tool-execution; the
    /// journal then contains a ToolCall whose ToolResultCommitted was
    /// never written. Strict validation (crash-recovery audits) keeps
    /// rejecting that shape, but resume must heal it or the session is
    /// permanently un-resumable.
    tolerant_orphans: bool,
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
            finished_not_committed: HashMap::new(),
            tolerant_orphans: false,
        }
    }

    /// Enable orphan-healing for resume paths (see field doc).
    pub fn tolerant_orphans(mut self) -> Self {
        self.tolerant_orphans = true;
        self
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
            RolloutEventType::ToolExecutionFinished => {
                // Capture the content from ToolExecutionFinished so that
                // if ToolResultCommitted never arrives (crash between the
                // two writes), finish() can synthesize a ToolResult from
                // the captured content rather than a generic "[interrupted]"
                // placeholder. This is the "Finished-not-Committed" path.
                if let Some(call_id_str) = event.payload.get("call_id").and_then(|v| v.as_str()) {
                    let content = event.payload.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let is_error = event.payload.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                    self.finished_not_committed.insert(call_id_str.to_string(), (content, is_error));
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
                    // Finished-not-Committed is now fully committed — remove
                    // from the interim map so finish() doesn't double-synthesize.
                    self.finished_not_committed.remove(call_id_str);
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
                // Validate no orphaned tool results. In tolerant mode
                // (resume) an orphan means the user interrupted mid-tool:
                // synthesize an error result so the transcript stays
                // call/result-paired instead of failing the whole resume.
                if !self.pending_tool_calls.is_empty() {
                    if self.tolerant_orphans {
                        self.heal_pending_orphans();
                    } else {
                        let orphaned: Vec<String> =
                            self.pending_tool_calls.keys().cloned().collect();
                        return Err(ReducerError::OrphanedToolResult(orphaned.join(", ")));
                    }
                }
            }
            RolloutEventType::ContextRestored => {
                // Context items restored from a prior session during resume.
                // Two journal shapes exist:
                //   1. Fork/boot-restore journals where ContextRestored is
                //      the FIRST context-producing event — context is empty,
                //      append everything.
                //   2. Legacy journals where resume re-wrote the FULL
                //      already-reconstructable context back into the SAME
                //      journal (snowball bug: +1 full copy per resume).
                //      Replaying the original events already rebuilt the
                //      context; appending the snapshot again would duplicate
                //      the transcript sent to the model.
                // Heuristic: when context is non-empty the snapshot is a
                // redundant re-write of state that the preceding events in
                // this same journal already reproduce — drop every item the
                // context already contains, keep anything genuinely new.
                // Membership is checked on the serialized form (HashSet)
                // because items can be hundreds of KB and a linear scan
                // would dominate resume time on legacy bloated journals.
                if let Some(items) = event.payload.get("items").and_then(|v| v.as_array()) {
                    use std::collections::HashSet;
                    let existing: HashSet<String> = if self.context.is_empty() {
                        HashSet::new()
                    } else {
                        self.context
                            .iter()
                            .filter_map(|c| serde_json::to_string(c).ok())
                            .collect()
                    };
                    for item_val in items {
                        if let Ok(item) = serde_json::from_value::<ContextItem>(item_val.clone()) {
                            let key = serde_json::to_string(&item).unwrap_or_default();
                            if self.context.is_empty() || !existing.contains(&key) {
                                self.context.push(item);
                            }
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

    /// Consume the reducer, healing interrupt-induced damage first when
    /// in tolerant mode:
    ///   1. Pending tool calls (journal ended before their result was
    ///      committed — the turn was aborted mid-execution) get a
    ///      synthetic error result so the transcript stays paired.
    ///   2. Reverse orphans — ToolResults with no preceding ToolCall
    ///      (malformed legacy journals) — are dropped, because wire
    ///      encoders reject a tool message with no matching tool_calls.
    /// In strict mode this is identical to `into_context`.
    pub fn finish(mut self) -> Vec<ContextItem> {
        if self.tolerant_orphans {
            if !self.pending_tool_calls.is_empty() {
                self.heal_pending_orphans();
            }
            self.drop_orphan_results();
        }
        self.context
    }

    /// Synthesize error results for every pending tool call, inserted
    /// directly after each call so transcript order stays natural.
    ///
    /// Two tiers of recovery:
    ///   1. **Finished-not-Committed**: the tool ran and the journal
    ///      captured its output (ToolExecutionFinished has content), but
    ///      ToolResultCommitted was never written. We synthesize from the
    ///      captured content — the model sees the real output.
    ///   2. **Dangling tool_call**: no Finished event either. The generic
    ///      "[interrupted]" message is used — outcome genuinely unknown.
    fn heal_pending_orphans(&mut self) {
        if self.pending_tool_calls.is_empty() {
            return;
        }
        let finished_count = self.pending_tool_calls.keys()
            .filter(|k| self.finished_not_committed.contains_key(k.as_str()))
            .count();
        tracing::warn!(
            orphaned = self.pending_tool_calls.len(),
            finished_not_committed = finished_count,
            "reducer: healing orphaned tool calls left by an interrupted turn"
        );
        let mut rebuilt = Vec::with_capacity(self.context.len() + self.pending_tool_calls.len());
        for item in std::mem::take(&mut self.context) {
            if let ContextItem::ToolCall { call_id, name, .. } = &item {
                if self.pending_tool_calls.remove(&call_id.to_string()).is_some() {
                    // Check if we have a Finished-not-Committed capture.
                    let synthetic = if let Some((content, is_error)) =
                        self.finished_not_committed.remove(&call_id.to_string())
                    {
                        // Tier 1: real content from ToolExecutionFinished.
                        tracing::info!(
                            call_id = %call_id,
                            content_len = content.len(),
                            "reducer: synthesized ToolResult from ToolExecutionFinished capture"
                        );
                        ContextItem::ToolResult {
                            call_id: *call_id,
                            content,
                            is_error,
                        }
                    } else {
                        // Tier 2: no capture — generic interrupted message.
                        ContextItem::ToolResult {
                            call_id: *call_id,
                            content: format!(
                                "[interrupted] Tool call `{name}` was interrupted by the user \
                                 before its result was committed; the outcome is unknown. \
                                 Do not assume it completed — verify actual state if needed."
                            ),
                            is_error: true,
                        }
                    };
                    rebuilt.push(item);
                    rebuilt.push(synthetic);
                    continue;
                }
            }
            rebuilt.push(item);
        }
        self.context = rebuilt;
        self.pending_tool_calls.clear();
        self.finished_not_committed.clear();
    }

    /// Drop ToolResults whose call_id never appeared as a ToolCall in
    /// the current context (reverse orphans).
    fn drop_orphan_results(&mut self) {
        let mut seen_calls: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut dropped = 0usize;
        self.context.retain(|item| match item {
            ContextItem::ToolCall { call_id, .. } => {
                seen_calls.insert(call_id.to_string());
                true
            }
            ContextItem::ToolResult { call_id, .. } => {
                let keep = seen_calls.contains(&call_id.to_string());
                if !keep {
                    dropped += 1;
                }
                keep
            }
            _ => true,
        });
        if dropped > 0 {
            tracing::warn!(dropped, "reducer: dropped orphaned tool results with no matching call");
        }
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

/// Replay a session journal WITHOUT materializing redundant
/// `ContextRestored` payloads, reducing the context in the same pass.
///
/// Legacy resumes re-wrote the ENTIRE restored context back into the
/// same journal on every `/resume` (snowball: 436 MB of duplicated
/// snapshots observed in one session). When replaying such a journal the
/// original events already reproduce the state, so those multi-MB
/// `items` arrays are pure waste. This reader detects them with a cheap
/// substring check + reducer-state probe and parses only a metadata
/// header for the skipped lines (the giant `payload` value is never
/// materialized as a `serde_json::Value`).
///
/// A `ContextRestored` event IS kept (fully parsed) when the reduced
/// context is still empty — that shape is a fork/boot-restore journal
/// whose leading snapshot is the ONLY source of the history.
///
/// Returns `(events, last_seq, reduced_context)`.
pub fn replay_journal_lean(
    jsonl_path: &std::path::Path,
    sid: &SessionId,
) -> Result<(Vec<RolloutEvent>, u64, Vec<ContextItem>), ReducerError> {
    use grodex_rollout::event::SensitivityLevel;

    /// Metadata-only view of a journal line. Fields not declared here —
    /// notably the giant `payload` — are tokenized and skipped by
    /// serde_json WITHOUT being materialized into a `Value`.
    #[derive(serde::Deserialize)]
    struct EventHeader {
        schema_version: u32,
        seq: u64,
        session_id: SessionId,
        #[serde(default)]
        turn_id: Option<TurnId>,
        #[serde(default)]
        step_id: Option<grodex_core::id::StepId>,
        #[serde(default)]
        generation: Option<StepGeneration>,
        timestamp: chrono::DateTime<chrono::Utc>,
        event_type: RolloutEventType,
        #[serde(default)]
        sensitivity: Option<SensitivityLevel>,
    }

    let mut events: Vec<RolloutEvent> = Vec::new();
    let mut last_seq = 0u64;
    // Tolerant: legacy journals frequently end with an interrupted turn
    // (ToolCall committed, ToolResultCommitted lost to the abort). Heal
    // those instead of failing the resume with OrphanedToolResult.
    let mut reducer = SessionReducer::new(*sid).tolerant_orphans();

    grodex_rollout::journal_actor::for_each_journal_line(jsonl_path, |line| {
        use grodex_core::error::GrodexError;
        // `event_type` serializes BEFORE the (multi-MB) `payload`, within
        // the first ~300 bytes of the line — probing only the head keeps
        // the skip check O(1) per line instead of scanning whole 32MB
        // lines. Lines where the head says "not ContextRestored" never
        // need another look.
        let mut head_len = line.len().min(300);
        while head_len > 0 && !line.is_char_boundary(head_len) {
            head_len -= 1;
        }
        let is_context_restored = line[..head_len]
            .contains("\"event_type\":\"ContextRestored\"");
        if is_context_restored && !reducer.context().is_empty() {
            // Redundant snapshot — parse metadata only so seq bookkeeping
            // stays intact without materializing the multi-MB payload.
            let hdr: EventHeader = serde_json::from_str(line).map_err(|e| {
                GrodexError::Internal(anyhow::anyhow!(
                    "resume: parse skipped ContextRestored header: {e}"
                ))
            })?;
            // Belt-and-suspenders: the head probe is a heuristic — verify
            // the real discriminant before dropping the payload.
            if hdr.event_type != RolloutEventType::ContextRestored {
                return Err(GrodexError::Internal(anyhow::anyhow!(
                    "resume: head probe misidentified event_type {:?}",
                    hdr.event_type
                )));
            }
            last_seq = last_seq.max(hdr.seq);
            let ev = RolloutEvent {
                schema_version: hdr.schema_version,
                seq: hdr.seq,
                session_id: hdr.session_id,
                turn_id: hdr.turn_id,
                step_id: hdr.step_id,
                generation: hdr.generation,
                timestamp: hdr.timestamp,
                event_type: RolloutEventType::ContextRestored,
                payload: serde_json::json!({ "items": [] }),
                sensitivity: hdr.sensitivity.unwrap_or(SensitivityLevel::Normal),
            };
            reducer
                .apply(&ev)
                .map_err(|e| GrodexError::Internal(anyhow::anyhow!("{e}")))?;
            events.push(ev);
            return Ok(());
        }
        let ev: RolloutEvent = serde_json::from_str(line).map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!("resume: journal parse: {e}"))
        })?;
        last_seq = last_seq.max(ev.seq);
        reducer
            .apply(&ev)
            .map_err(|e| GrodexError::Internal(anyhow::anyhow!("{e}")))?;
        events.push(ev);
        Ok(())
    })
    .map_err(|e| ReducerError::JournalRead(e.to_string()))?;

    let ctx = reducer.finish();
    Ok((events, last_seq, ctx))
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

    /// ContextRestored written back into the SAME journal (legacy
    /// snowball bug) must not duplicate the transcript on replay.
    #[test]
    fn context_restored_dedup_on_full_replay() {
        let sid = SessionId::new();
        let user_item = ContextItem::User {
            content: "hello".into(),
            message_id: None,
        };
        let assistant_item = ContextItem::Assistant { content: "hi there".into() };
        let events = vec![
            make_event(sid, 0, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "hello"})),
            make_event(sid, 1, RolloutEventType::ModelItemProduced, serde_json::json!({"assistant_text": "hi there"})),
            make_event(sid, 2, RolloutEventType::TurnCompleted, serde_json::json!({})),
            // Redundant snapshot of the already-reconstructable context.
            make_event(sid, 3, RolloutEventType::ContextRestored, serde_json::json!({
                "items": [serde_json::to_value(&user_item).unwrap(), serde_json::to_value(&assistant_item).unwrap()]
            })),
        ];

        let mut reducer = SessionReducer::new(sid);
        reducer.apply_all(&events).unwrap();
        let ctx = reducer.into_context();
        // Must stay 2 — the snapshot duplicates items 0..=1.
        assert_eq!(ctx.len(), 2, "ContextRestored snapshot must not duplicate history");
    }

    /// Fork/boot-restore journals START with ContextRestored (context
    /// empty) — those items are the only history source and must be kept.
    #[test]
    fn leading_context_restored_kept() {
        let sid = SessionId::new();
        let user_item = ContextItem::User {
            content: "prior history".into(),
            message_id: None,
        };
        let events = vec![
            make_event(sid, 0, RolloutEventType::ContextRestored, serde_json::json!({
                "items": [serde_json::to_value(&user_item).unwrap()]
            })),
            make_event(sid, 1, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "new turn"})),
            make_event(sid, 2, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];

        let mut reducer = SessionReducer::new(sid);
        reducer.apply_all(&events).unwrap();
        let ctx = reducer.into_context();
        assert_eq!(ctx.len(), 2);
        assert!(matches!(ctx[0], ContextItem::User { ref content, .. } if content == "prior history"));
        assert!(matches!(ctx[1], ContextItem::User { ref content, .. } if content == "new turn"));
    }

    fn write_journal(dir: &tempfile::TempDir, sid: &SessionId, events: &[RolloutEvent]) -> std::path::PathBuf {
        let session_dir = dir.path().join(sid.to_string());
        std::fs::create_dir_all(&session_dir).unwrap();
        let jsonl = session_dir.join("rollout.jsonl");
        let mut content = String::new();
        for ev in events {
            content.push_str(&serde_json::to_string(ev).unwrap());
            content.push('\n');
        }
        std::fs::write(&jsonl, content).unwrap();
        jsonl
    }

    /// End-to-end lean replay over a legacy-shaped journal file: the
    /// redundant ContextRestored payload is skipped, context stays
    /// duplicate-free, and last_seq still covers the skipped event.
    #[test]
    fn lean_replay_skips_redundant_snapshots() {
        let sid = SessionId::new();
        let user_item = ContextItem::User {
            content: "hello".into(),
            message_id: None,
        };
        let events = vec![
            make_event(sid, 0, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "hello"})),
            make_event(sid, 1, RolloutEventType::ModelItemProduced, serde_json::json!({"assistant_text": "hi there"})),
            make_event(sid, 2, RolloutEventType::TurnCompleted, serde_json::json!({})),
            // Giant redundant snapshot (same items the events above rebuild).
            make_event(sid, 3, RolloutEventType::ContextRestored, serde_json::json!({
                "items": [serde_json::to_value(&user_item).unwrap()]
            })),
            make_event(sid, 4, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "second"})),
            make_event(sid, 5, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];
        let dir = tempfile::tempdir().unwrap();
        let jsonl = write_journal(&dir, &sid, &events);

        let (lean_events, last_seq, ctx) = replay_journal_lean(&jsonl, &sid).unwrap();
        assert_eq!(last_seq, 5);
        assert_eq!(lean_events.len(), 6);
        // The skipped snapshot must carry an EMPTY payload.
        assert_eq!(lean_events[3].payload["items"].as_array().unwrap().len(), 0);
        // user + assistant + user — no duplicates from the snapshot.
        assert_eq!(ctx.len(), 3);
    }

    /// Lean replay keeps a LEADING ContextRestored (fork journal shape).
    #[test]
    fn lean_replay_keeps_leading_context_restored() {
        let sid = SessionId::new();
        let user_item = ContextItem::User {
            content: "prior history".into(),
            message_id: None,
        };
        let events = vec![
            make_event(sid, 0, RolloutEventType::ContextRestored, serde_json::json!({
                "items": [serde_json::to_value(&user_item).unwrap()]
            })),
            make_event(sid, 1, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "new turn"})),
            make_event(sid, 2, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];
        let dir = tempfile::tempdir().unwrap();
        let jsonl = write_journal(&dir, &sid, &events);

        let (lean_events, last_seq, ctx) = replay_journal_lean(&jsonl, &sid).unwrap();
        assert_eq!(last_seq, 2);
        assert_eq!(lean_events.len(), 3);
        assert_eq!(ctx.len(), 2);
        assert!(matches!(ctx[0], ContextItem::User { ref content, .. } if content == "prior history"));
    }

    /// Strict mode (crash-recovery audits) still rejects a ToolCall that
    /// never received a result before TurnCompleted.
    #[test]
    fn strict_mode_rejects_orphaned_tool_call() {
        let sid = SessionId::new();
        let cid = grodex_core::id::ToolCallId::new();
        let events = vec![
            make_event(sid, 0, RolloutEventType::ModelItemProduced, serde_json::json!({
                "tool_calls": [{"call_id": cid.to_string(), "name": "exec", "arguments": {}}]
            })),
            make_event(sid, 1, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];
        let mut reducer = SessionReducer::new(sid);
        let err = reducer.apply_all(&events).unwrap_err();
        assert!(matches!(err, ReducerError::OrphanedToolResult(_)));
    }

    /// Tolerant mode (resume): the same interrupted-turn shape heals
    /// into a call + synthetic error result instead of failing.
    #[test]
    fn tolerant_mode_heals_orphan_at_turn_boundary() {
        let sid = SessionId::new();
        let cid = grodex_core::id::ToolCallId::new();
        let events = vec![
            make_event(sid, 0, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "run tests"})),
            make_event(sid, 1, RolloutEventType::ModelItemProduced, serde_json::json!({
                "tool_calls": [{"call_id": cid.to_string(), "name": "exec", "arguments": {}}]
            })),
            make_event(sid, 2, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];
        let mut reducer = SessionReducer::new(sid).tolerant_orphans();
        reducer.apply_all(&events).unwrap();
        let ctx = reducer.finish();

        // user + tool call + synthetic result.
        assert_eq!(ctx.len(), 3);
        assert!(matches!(ctx[1], ContextItem::ToolCall { .. }));
        match &ctx[2] {
            ContextItem::ToolResult { call_id, content, is_error } => {
                assert_eq!(call_id, &cid);
                assert!(is_error);
                assert!(content.contains("interrupted"));
            }
            other => panic!("expected synthetic ToolResult, got {other:?}"),
        }
    }

    /// `finish()` heals a journal that ENDS mid-tool (no TurnCompleted
    /// after the abort) and drops reverse orphans (results with no call).
    #[test]
    fn tolerant_finish_heals_tail_and_drops_reverse_orphans() {
        let sid = SessionId::new();
        let cid = grodex_core::id::ToolCallId::new();
        let stray = grodex_core::id::ToolCallId::new();
        let events = vec![
            // Reverse orphan: result whose call event was lost.
            make_event(sid, 0, RolloutEventType::ToolResultCommitted, serde_json::json!({
                "call_id": stray.to_string(), "content": "lost call", "is_error": false
            })),
            make_event(sid, 1, RolloutEventType::ModelItemProduced, serde_json::json!({
                "tool_calls": [{"call_id": cid.to_string(), "name": "exec", "arguments": {}}]
            })),
            // Journal ends here — the turn was aborted, no TurnCompleted.
        ];
        let mut reducer = SessionReducer::new(sid).tolerant_orphans();
        reducer.apply_all(&events).unwrap();
        let ctx = reducer.finish();

        // stray result dropped; call + synthetic result kept.
        assert_eq!(ctx.len(), 2);
        assert!(matches!(ctx[0], ContextItem::ToolCall { .. }));
        assert!(matches!(ctx[1], ContextItem::ToolResult { is_error: true, .. }));
    }

    /// End-to-end: lean replay of an interrupted-turn journal succeeds
    /// and yields a fully call/result-paired transcript.
    #[test]
    fn lean_replay_heals_interrupted_journal() {
        let sid = SessionId::new();
        let cid = grodex_core::id::ToolCallId::new();
        let events = vec![
            make_event(sid, 0, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "run tests"})),
            make_event(sid, 1, RolloutEventType::ModelItemProduced, serde_json::json!({
                "tool_calls": [{"call_id": cid.to_string(), "name": "exec", "arguments": {}}]
            })),
            // User hit Esc mid-execution: no ToolResultCommitted, and a
            // LATER turn's TurnCompleted surfaces the orphan.
            make_event(sid, 2, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "continue"})),
            make_event(sid, 3, RolloutEventType::TurnCompleted, serde_json::json!({})),
        ];
        let dir = tempfile::tempdir().unwrap();
        let jsonl = write_journal(&dir, &sid, &events);

        let (_evts, last_seq, ctx) = replay_journal_lean(&jsonl, &sid).unwrap();
        assert_eq!(last_seq, 3);
        // user + call + synthetic result + user.
        assert_eq!(ctx.len(), 4);
        assert!(matches!(ctx[2], ContextItem::ToolResult { is_error: true, .. }));
    }
}
