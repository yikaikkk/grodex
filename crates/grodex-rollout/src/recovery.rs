use crate::event::RolloutEvent;
use crate::event::RolloutEventType;
use grodex_core::id::ToolCallId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecoveryCheckpoint {
    pub last_committed_seq: u64,
    pub in_flight_tool_call_ids: BTreeSet<String>,
    pub already_executed_operation_ids: BTreeSet<String>,
    pub closed_turns: BTreeSet<String>,
}

pub fn recover_from_journal(events: &[RolloutEvent]) -> RecoveryCheckpoint {
    let mut cp = RecoveryCheckpoint::default();
    for ev in events {
        let id_opt = || ev.payload.get("tool_call_id").and_then(|v| v.as_str()).map(|s| s.to_string());
        let op_id_opt = || ev.payload.get("operation_id").and_then(|v| v.as_str()).map(|s| s.to_string());
        let turn_opt = || ev.turn_id.as_ref().map(|s| s.to_string());
        match ev.event_type {
            RolloutEventType::ToolExecutionStarted => {
                if let Some(id) = id_opt() {
                    cp.in_flight_tool_call_ids.insert(id);
                }
            }
            RolloutEventType::ToolExecutionFinished | RolloutEventType::ToolResultCommitted => {
                if let Some(id) = id_opt() { cp.in_flight_tool_call_ids.remove(&id); }
                if let Some(opid) = op_id_opt() { cp.already_executed_operation_ids.insert(opid); }
                if matches!(ev.event_type, RolloutEventType::ToolResultCommitted) {
                    cp.last_committed_seq = cp.last_committed_seq.max(ev.seq);
                }
            }
            RolloutEventType::TurnCompleted => {
                if let Some(tid) = turn_opt() { cp.closed_turns.insert(tid); }
                cp.last_committed_seq = cp.last_committed_seq.max(ev.seq);
            }
            _ => {}
        }
    }
    cp
}

impl RecoveryCheckpoint {
    pub fn is_safe_to_replay(&self, operation_id: &str) -> bool {
        !self.already_executed_operation_ids.contains(operation_id)
    }
    pub fn is_in_flight(&self, call_id: &ToolCallId) -> bool {
        self.in_flight_tool_call_ids.contains(&call_id.to_string())
    }
    pub fn is_turn_closed(&self, turn_id: &str) -> bool { self.closed_turns.contains(turn_id) }
}
