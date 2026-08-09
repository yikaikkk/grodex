use crate::event::{RolloutEvent, RolloutEventType};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantFailure {
    pub code: String,
    pub message: String,
    pub event_seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InvariantReport {
    pub failures: Vec<InvariantFailure>,
    pub warnings: Vec<InvariantWarning>,
}

impl InvariantReport {
    pub fn merge(&mut self, other: InvariantReport) {
        self.failures.extend(other.failures);
        self.warnings.extend(other.warnings);
    }
    pub fn ok(&self) -> bool { self.failures.is_empty() }
}

pub trait InvariantAssertion {
    fn code(&self) -> &'static str;
    fn check(&self, events: &[RolloutEvent]) -> InvariantReport;
}

pub struct ToolCallLifecycleConsistency;
impl InvariantAssertion for ToolCallLifecycleConsistency {
    fn code(&self) -> &'static str { "TOOL_CALL_LIFECYCLE" }
    fn check(&self, events: &[RolloutEvent]) -> InvariantReport {
        let mut report = InvariantReport::default();
        let mut prepared: HashMap<String, (u64, usize)> = HashMap::new();
        let mut started: HashSet<String> = HashSet::new();
        let mut finished: HashSet<String> = HashSet::new();
        for (i, ev) in events.iter().enumerate() {
            let id_opt = || ev.payload.get("tool_call_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            match ev.event_type {
                RolloutEventType::ToolCallPrepared => {
                    if let Some(id) = id_opt() {
                        prepared.entry(id).or_insert((ev.seq, i));
                    }
                }
                RolloutEventType::ToolExecutionStarted => {
                    if let Some(id) = id_opt() { started.insert(id); }
                }
                RolloutEventType::ToolExecutionFinished | RolloutEventType::ToolResultCommitted => {
                    if let Some(id) = id_opt() { finished.insert(id); }
                }
                _ => {}
            }
        }
        for (id, (seq, _idx)) in &prepared {
            if !started.contains(id) {
                report.failures.push(InvariantFailure {
                    code: self.code().to_string(),
                    message: format!("tool call {} (seq={}) was Prepared but never had ToolExecutionStarted", id, seq),
                    event_seq: Some(*seq),
                });
            }
        }
        report
    }
}

pub struct StepGenerationMonotonic;
impl InvariantAssertion for StepGenerationMonotonic {
    fn code(&self) -> &'static str { "STEP_GENERATION_MONOTONIC" }
    fn check(&self, events: &[RolloutEvent]) -> InvariantReport {
        let mut report = InvariantReport::default();
        let mut per_step: HashMap<String, (u64, u64)> = HashMap::new();
        for ev in events {
            let Some(step_id) = ev.step_id.as_ref().map(|s| s.to_string()) else { continue };
            let Some(generation) = ev.generation else { continue };
            let gen_val = generation.as_u64();
            let entry = per_step.entry(step_id.clone()).or_insert((gen_val, ev.seq));
            if gen_val < entry.0 {
                report.failures.push(InvariantFailure {
                    code: self.code().to_string(),
                    message: format!("step {} generation went backwards: {} < previous {} at seq={}", step_id, gen_val, entry.0, ev.seq),
                    event_seq: Some(ev.seq),
                });
            } else {
                entry.0 = gen_val;
            }
        }
        report
    }
}

pub struct CompactionAtomicity;
impl InvariantAssertion for CompactionAtomicity {
    fn code(&self) -> &'static str { "COMPACTION_ATOMICITY" }
    fn check(&self, events: &[RolloutEvent]) -> InvariantReport {
        let mut report = InvariantReport::default();
        let mut open_starts = Vec::new();
        for ev in events {
            match ev.event_type {
                RolloutEventType::CompactionStarted => open_starts.push(ev.seq),
                RolloutEventType::CompactionCommitted | RolloutEventType::CompactionFailed => { open_starts.pop(); }
                _ => {}
            }
        }
        if !open_starts.is_empty() {
            report.failures.push(InvariantFailure {
                code: self.code().to_string(),
                message: format!("{} unclosed CompactionStarted events; earliest seq={}",
                    open_starts.len(), open_starts[0]),
                event_seq: Some(open_starts[0]),
            });
        }
        report
    }
}

pub struct NoCommittedBeforePrepared;
impl InvariantAssertion for NoCommittedBeforePrepared {
    fn code(&self) -> &'static str { "COMMIT_ORDER" }
    fn check(&self, events: &[RolloutEvent]) -> InvariantReport {
        let mut report = InvariantReport::default();
        let mut prepared_seen = HashSet::new();
        for ev in events {
            let id_opt = || ev.payload.get("tool_call_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            match ev.event_type {
                RolloutEventType::ToolCallPrepared => {
                    if let Some(id) = id_opt() { prepared_seen.insert(id); }
                }
                RolloutEventType::ToolResultCommitted => {
                    if let Some(id) = id_opt() {
                        if !prepared_seen.contains(&id) {
                            report.failures.push(InvariantFailure {
                                code: self.code().to_string(),
                                message: format!("ToolResultCommitted for {} before any ToolCallPrepared event; seq={}", id, ev.seq),
                                event_seq: Some(ev.seq),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        report
    }
}

pub struct TurnCompletionClosure;
impl InvariantAssertion for TurnCompletionClosure {
    fn code(&self) -> &'static str { "TURN_COMPLETION_CLOSURE" }
    fn check(&self, events: &[RolloutEvent]) -> InvariantReport {
        let mut report = InvariantReport::default();
        let mut closed_turns = HashSet::new();
        for ev in events {
            let Some(turn_id) = ev.turn_id.as_ref().map(|s| s.to_string()) else { continue };
            if matches!(ev.event_type, RolloutEventType::TurnCompleted) {
                closed_turns.insert(turn_id.clone());
                continue;
            }
            if closed_turns.contains(&turn_id) {
                let forbidden = !matches!(ev.event_type,
                    RolloutEventType::RuntimeStateChanged | RolloutEventType::TurnCompleted);
                if forbidden {
                    report.failures.push(InvariantFailure {
                        code: self.code().to_string(),
                        message: format!("event {:?} appeared after TurnCompleted for turn {}; seq={}",
                            ev.event_type, turn_id, ev.seq),
                        event_seq: Some(ev.seq),
                    });
                }
            }
        }
        report
    }
}

pub fn run_all_invariants(events: &[RolloutEvent]) -> InvariantReport {
    let all: Vec<Box<dyn InvariantAssertion>> = vec![
        Box::new(ToolCallLifecycleConsistency),
        Box::new(StepGenerationMonotonic),
        Box::new(CompactionAtomicity),
        Box::new(NoCommittedBeforePrepared),
        Box::new(TurnCompletionClosure),
    ];
    let mut out = InvariantReport::default();
    for a in all { out.merge(a.check(events)); }
    out
}

pub fn run_owned<T: IntoIterator<Item = RolloutEvent>>(events: T) -> InvariantReport {
    let collected: Vec<RolloutEvent> = events.into_iter().collect();
    run_all_invariants(&collected)
}

pub fn run_for_loom_testing<T: IntoIterator<Item = RolloutEvent>>(events: T) -> InvariantReport {
    run_owned(events)
}
