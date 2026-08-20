use crate::event::RolloutEvent;
use crate::event::RolloutEventType;
use grodex_core::id::ToolCallId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// How the recovery algorithm classified a single tool call after
/// scanning the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallFate {
    /// Prepared + Approved + Started + Finished + Committed — the call
    /// reached a fully-durable terminal state. It is safe to drop the
    /// call on resume.
    Completed,
    /// Prepared + Approved + Started, but neither Finished nor Result
    /// are present. This is the hallmark of a crash DURING side-effect
    /// execution. The only correct next step is to write a
    /// `ToolOutcomeIndeterminate` event, surface it, and wait for a
    /// human (or trusted external arbiter) to emit a
    /// `ToolOutcomeResolved`. Auto-replay is FORBIDDEN in this state.
    Indeterminate,
    /// Prepared but no Approved / Started yet. The tool never executed,
    /// so it is safe to resubmit as part of normal resume without a
    /// human decision.
    NotStarted,
    /// A previous recovery already marked this call `Indeterminate`
    /// and a human responded with a resolution. Replay-safe.
    Resolved,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecoveryCheckpoint {
    pub last_committed_seq: u64,
    /// call_ids currently in flight (Started, no Finished). After a
    /// crash these are candidates for `Indeterminate` classification.
    pub in_flight_call_ids: BTreeSet<String>,
    /// operation_ids that have reached a durable terminal state
    /// (ToolResultCommitted | ToolOutcomeResolved::confirmed_executed).
    /// Tools that accept an idempotency key MUST refuse to re-run when
    /// their operation_id is in this set.
    pub already_executed_operation_ids: BTreeSet<String>,
    pub closed_turns: BTreeSet<String>,
    /// Per-call_id fate, derived by applying all of:
    ///   Prepared → Approved → Started → Finished → ResultCommitted
    /// together with the Indeterminate / Resolved override path.
    pub call_fate: BTreeMap<String, ToolCallFate>,
    /// Leases that have been consumed (LeaseConsumed observed).
    /// Duplicate lease consumption must be rejected by the executor.
    pub consumed_leases: BTreeSet<String>,
    /// Approval tickets that have already been resolved (so we don't
    /// ask the user twice on resume — the durable answer is in the
    /// journal).
    pub resolved_tickets: BTreeMap<String, ApprovalTicketResolution>,
    /// All approval tickets that were requested (ApprovalRequested
    /// observed) — used together with `resolved_tickets` to compute
    /// the set of *pending* (unresolved) tickets that must be
    /// re-surfaced to the frontend on resume.
    pub requested_tickets: BTreeMap<String, RequestedTicketInfo>,
}

/// Terminal state of an approval ticket, reconstructed from the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalTicketResolution {
    pub resolution: String, // "approved" | "rejected" | "expired" | "narrowed"
    pub narrowed_args: Option<serde_json::Value>,
    pub call_id: Option<String>,
    pub resolved_by: Option<String>,
}

/// Info about an approval ticket that was requested (ApprovalRequested
/// event observed) — used to re-surface unresolved tickets to the
/// frontend during resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestedTicketInfo {
    pub ticket_id: String,
    pub tool_name: String,
    pub call_id: Option<String>,
    pub args: Option<serde_json::Value>,
}

/// Field accessors — unified schema v2.
///
/// Journal writer writes `call_id` (not `tool_call_id`). We still fall
/// back to `tool_call_id` for v1 journals written before this rewrite,
/// but the preferred key is `call_id`.
fn call_id(ev: &RolloutEvent) -> Option<String> {
    ev.payload
        .get("call_id")
        .and_then(|v| v.as_str())
        .or_else(|| ev.payload.get("tool_call_id").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}
fn operation_id(ev: &RolloutEvent) -> Option<String> {
    ev.payload
        .get("operation_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn recover_from_journal(events: &[RolloutEvent]) -> RecoveryCheckpoint {
    let mut cp = RecoveryCheckpoint::default();
    for ev in events {
        let cid = call_id(ev);
        let opid = operation_id(ev);
        let turn = ev.turn_id.as_ref().map(|s| s.to_string());

        match &ev.event_type {
            RolloutEventType::ToolCallPrepared => {
                if let Some(c) = &cid {
                    // Don't overwrite a stronger fate (e.g. after replay
                    // reorder, which shouldn't happen with strict seq,
                    // but be defensive).
                    cp.call_fate.entry(c.clone())
                        .or_insert(ToolCallFate::NotStarted);
                }
            }
            RolloutEventType::ToolCallApproved => {
                // Approved but not yet Started — still NotStarted.
                if let Some(c) = &cid {
                    if !matches!(
                        cp.call_fate.get(c),
                        Some(ToolCallFate::Completed)
                            | Some(ToolCallFate::Indeterminate)
                            | Some(ToolCallFate::Resolved)
                    ) {
                        cp.call_fate.insert(c.clone(), ToolCallFate::NotStarted);
                    }
                }
            }
            RolloutEventType::ToolExecutionStarted => {
                if let Some(c) = &cid {
                    cp.in_flight_call_ids.insert(c.clone());
                    // Indeterminate is only written AFTER a crash; a
                    // live Started during the same run is still
                    // "not-yet-finished", not indeterminate.
                    if matches!(cp.call_fate.get(c), None | Some(ToolCallFate::NotStarted)) {
                        // Keep as NotStarted internally; the *presence*
                        // in in_flight_call_ids is what drives the
                        // post-crash Indeterminate classification.
                    }
                }
            }
            RolloutEventType::ToolExecutionFinished => {
                if let Some(c) = &cid {
                    cp.in_flight_call_ids.remove(c);
                }
            }
            RolloutEventType::ToolResultCommitted => {
                if let Some(c) = &cid {
                    cp.in_flight_call_ids.remove(c);
                    cp.call_fate.insert(c.clone(), ToolCallFate::Completed);
                }
                if let Some(op) = opid {
                    cp.already_executed_operation_ids.insert(op);
                }
                cp.last_committed_seq = cp.last_committed_seq.max(ev.seq);
            }
            RolloutEventType::ToolOutcomeIndeterminate => {
                if let Some(c) = &cid {
                    cp.in_flight_call_ids.remove(c);
                    cp.call_fate.insert(c.clone(), ToolCallFate::Indeterminate);
                }
            }
            RolloutEventType::ToolOutcomeResolved => {
                if let Some(c) = &cid {
                    cp.in_flight_call_ids.remove(c);
                    cp.call_fate.insert(c.clone(), ToolCallFate::Resolved);
                }
                // If the human confirmed the side-effect actually
                // happened, mark the operation_id executed so dedup
                // works for tools that check it.
                let confirmed_executed = ev.payload
                    .get("resolution")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "confirmed_executed")
                    .unwrap_or(false);
                if confirmed_executed {
                    if let Some(op) = opid {
                        cp.already_executed_operation_ids.insert(op);
                    }
                }
                cp.last_committed_seq = cp.last_committed_seq.max(ev.seq);
            }
            RolloutEventType::LeaseConsumed => {
                if let Some(lease_id) = ev.payload.get("lease_id").and_then(|v| v.as_str()) {
                    cp.consumed_leases.insert(lease_id.to_string());
                }
            }
            RolloutEventType::ApprovalRequested => {
                if let Some(ticket_id) = ev.payload.get("ticket_id").and_then(|v| v.as_str()) {
                    let tool_name = ev.payload
                        .get("tool_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let call_id = ev.payload
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let args = ev.payload.get("args").cloned();
                    cp.requested_tickets.insert(
                        ticket_id.to_string(),
                        RequestedTicketInfo {
                            ticket_id: ticket_id.to_string(),
                            tool_name,
                            call_id,
                            args,
                        },
                    );
                }
            }
            RolloutEventType::ApprovalResolved => {
                if let Some(ticket_id) = ev.payload.get("ticket_id").and_then(|v| v.as_str()) {
                    let resolution = ev.payload
                        .get("resolution")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let narrowed_args = ev.payload.get("narrowed_args").cloned();
                    let call_id_from_payload = ev.payload
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let resolved_by = ev.payload
                        .get("resolved_by")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    cp.resolved_tickets.insert(
                        ticket_id.to_string(),
                        ApprovalTicketResolution {
                            resolution,
                            narrowed_args,
                            call_id: call_id_from_payload,
                            resolved_by,
                        },
                    );
                }
            }
            RolloutEventType::TurnCompleted => {
                if let Some(tid) = turn {
                    cp.closed_turns.insert(tid);
                }
                cp.last_committed_seq = cp.last_committed_seq.max(ev.seq);
            }
            _ => {}
        }
    }

    // Final classification pass: anything still in in_flight_call_ids
    // at the end of the scan = started without finished → Indeterminate.
    for c in cp.in_flight_call_ids.iter().cloned().collect::<Vec<_>>() {
        if matches!(cp.call_fate.get(&c), None | Some(ToolCallFate::NotStarted)) {
            cp.call_fate.insert(c, ToolCallFate::Indeterminate);
        }
    }

    cp
}

impl RecoveryCheckpoint {
    pub fn is_safe_to_replay(&self, operation_id: &str) -> bool {
        !self.already_executed_operation_ids.contains(operation_id)
    }
    pub fn is_in_flight(&self, call_id: &ToolCallId) -> bool {
        self.in_flight_call_ids.contains(&call_id.to_string())
    }
    pub fn is_turn_closed(&self, turn_id: &str) -> bool { self.closed_turns.contains(turn_id) }

    pub fn fate_of(&self, call_id: &str) -> ToolCallFate {
        self.call_fate.get(call_id).copied().unwrap_or(ToolCallFate::NotStarted)
    }
    pub fn is_lease_consumed(&self, lease_id: &str) -> bool {
        self.consumed_leases.contains(lease_id)
    }
    pub fn ticket_resolution(&self, ticket_id: &str) -> Option<&ApprovalTicketResolution> {
        self.resolved_tickets.get(ticket_id)
    }
    pub fn indeterminate_call_ids(&self) -> Vec<String> {
        self.call_fate
            .iter()
            .filter(|(_, f)| **f == ToolCallFate::Indeterminate)
            .map(|(c, _)| c.clone())
            .collect()
    }
    /// Approval tickets that were requested but never resolved — these
    /// must be re-surfaced to the frontend on resume so the user can
    /// adjudicate them.
    pub fn pending_approval_tickets(&self) -> Vec<&RequestedTicketInfo> {
        self.requested_tickets
            .iter()
            .filter(|(tid, _)| !self.resolved_tickets.contains_key(tid.as_str()))
            .map(|(_, info)| info)
            .collect()
    }
}
