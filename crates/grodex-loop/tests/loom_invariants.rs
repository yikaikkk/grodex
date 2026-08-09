#[cfg(not(loom))]
#[test]
fn placeholder_requires_cfg_loom_flag() {
    assert!(true);
    println!(
        "Rerun with RUSTFLAGS=\"--cfg loom\" cargo test -p grodex-loop --test loom_invariants to run"
    );
}

#[cfg(loom)]
mod model {
    use loom::sync::atomic::{AtomicU64, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;
    use std::collections::HashMap;

    #[derive(Debug, Clone)]
    pub enum ModelEvent {
        ToolCallStarted {
            tool_call_id: String,
            operation_id: String,
            generation: u64,
            seq: u64,
        },
        ToolCallResultCommitted {
            tool_call_id: String,
            operation_id: String,
            generation: u64,
            seq: u64,
            exit_ok: bool,
        },
        ToolCallCancelled {
            tool_call_id: String,
            operation_id: String,
            reason: &'static str,
            seq: u64,
        },
        GenerationBumped {
            generation: u64,
            seq: u64,
        },
    }

    impl ModelEvent {
        fn generation(&self) -> Option<u64> {
            match self {
                ModelEvent::ToolCallStarted { generation, .. }
                | ModelEvent::ToolCallResultCommitted { generation, .. } => Some(*generation),
                ModelEvent::GenerationBumped { generation, .. } => Some(*generation),
                ModelEvent::ToolCallCancelled { .. } => None,
            }
        }
        fn tool_call_id(&self) -> Option<&str> {
            match self {
                ModelEvent::ToolCallStarted { tool_call_id, .. }
                | ModelEvent::ToolCallResultCommitted { tool_call_id, .. }
                | ModelEvent::ToolCallCancelled { tool_call_id, .. } => Some(tool_call_id.as_str()),
                ModelEvent::GenerationBumped { .. } => None,
            }
        }
        fn seq(&self) -> u64 {
            match self {
                ModelEvent::ToolCallStarted { seq, .. }
                | ModelEvent::ToolCallResultCommitted { seq, .. }
                | ModelEvent::ToolCallCancelled { seq, .. }
                | ModelEvent::GenerationBumped { seq, .. } => *seq,
            }
        }
        fn with_seq(self, new_seq: u64) -> Self {
            match self {
                ModelEvent::ToolCallStarted {
                    tool_call_id,
                    operation_id,
                    generation,
                    ..
                } => ModelEvent::ToolCallStarted {
                    tool_call_id,
                    operation_id,
                    generation,
                    seq: new_seq,
                },
                ModelEvent::ToolCallResultCommitted {
                    tool_call_id,
                    operation_id,
                    generation,
                    exit_ok,
                    ..
                } => ModelEvent::ToolCallResultCommitted {
                    tool_call_id,
                    operation_id,
                    generation,
                    seq: new_seq,
                    exit_ok,
                },
                ModelEvent::ToolCallCancelled {
                    tool_call_id,
                    operation_id,
                    reason,
                    ..
                } => ModelEvent::ToolCallCancelled {
                    tool_call_id,
                    operation_id,
                    reason,
                    seq: new_seq,
                },
                ModelEvent::GenerationBumped { generation, .. } => ModelEvent::GenerationBumped {
                    generation,
                    seq: new_seq,
                },
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct InvariantReportModel {
        pub all_ok: bool,
        pub violations: Vec<String>,
    }

    impl InvariantReportModel {
        fn ok() -> Self {
            Self {
                all_ok: true,
                violations: Vec::new(),
            }
        }
        fn push(&mut self, msg: impl Into<String>) {
            self.all_ok = false;
            self.violations.push(msg.into());
        }
    }

    pub struct SharedJournal {
        seq_counter: AtomicU64,
        id_allocator: AtomicU64,
        events: Mutex<Vec<(u64, ModelEvent)>>,
        current_generation: AtomicU64,
    }

    impl SharedJournal {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                seq_counter: AtomicU64::new(1),
                id_allocator: AtomicU64::new(1),
                events: Mutex::new(vec![]),
                current_generation: AtomicU64::new(1),
            })
        }

        fn next_id(&self) -> u64 {
            self.id_allocator.fetch_add(1, Ordering::SeqCst)
        }

        fn push(&self, ev: ModelEvent) -> u64 {
            let s = self.seq_counter.fetch_add(1, Ordering::SeqCst);
            let ev_with_seq = ev.with_seq(s);
            self.events.lock().unwrap().push((s, ev_with_seq));
            s
        }

        fn snapshot_sorted(&self) -> Vec<ModelEvent> {
            let mut events = self.events.lock().unwrap().clone();
            events.sort_by_key(|(s, _)| *s);
            events.into_iter().map(|(_, e)| e).collect()
        }
    }

    pub fn worker_execute_tools(j: Arc<SharedJournal>, count: u8) {
        for i in 0..count {
            let unique = j.next_id();
            let id = format!("tc_{unique}_{i}");
            let op_id = format!("op_{unique}_{i}");
            let gen_val = j.current_generation.load(Ordering::SeqCst);
            j.push(ModelEvent::ToolCallStarted {
                tool_call_id: id.clone(),
                operation_id: op_id.clone(),
                generation: gen_val,
                seq: 0,
            });
            thread::yield_now();
            thread::yield_now();
            let gen_after = j.current_generation.load(Ordering::SeqCst);
            j.push(ModelEvent::ToolCallResultCommitted {
                tool_call_id: id,
                operation_id: op_id,
                generation: gen_after,
                seq: 0,
                exit_ok: i % 3 != 0,
            });
        }
    }

    pub fn worker_occasionally_cancel(j: Arc<SharedJournal>) {
        thread::yield_now();
        thread::yield_now();
        thread::yield_now();
        thread::yield_now();

        let candidate_id: Option<String> = {
            let evs = j.events.lock().unwrap();
            let mut started_ids: Vec<String> = Vec::new();
            for (_, ev) in evs.iter() {
                if let Some(id) = ev.tool_call_id() {
                    started_ids.push(id.to_string());
                }
            }
            started_ids.into_iter().next()
        };

        if let Some(tid) = candidate_id {
            let op_id = format!("op_cancel_{tid}");
            j.push(ModelEvent::ToolCallCancelled {
                tool_call_id: tid,
                operation_id: op_id,
                reason: "cancel",
                seq: 0,
            });
        } else {
            let unique = j.next_id();
            let id = format!("tc_cancel_fallback_{unique}");
            let op_id = format!("op_cancel_{unique}");
            let gen_val = j.current_generation.load(Ordering::SeqCst);
            j.push(ModelEvent::ToolCallStarted {
                tool_call_id: id.clone(),
                operation_id: op_id.clone(),
                generation: gen_val,
                seq: 0,
            });
            thread::yield_now();
            j.push(ModelEvent::ToolCallCancelled {
                tool_call_id: id,
                operation_id: op_id,
                reason: "cancel",
                seq: 0,
            });
        }
    }

    pub fn worker_revocation_bump(j: Arc<SharedJournal>) {
        thread::yield_now();
        thread::yield_now();
        let new = j.current_generation.fetch_add(1, Ordering::SeqCst) + 1;
        j.push(ModelEvent::GenerationBumped {
            generation: new,
            seq: 0,
        });
    }

    pub fn worker_timeout(j: Arc<SharedJournal>) {
        thread::yield_now();
        thread::yield_now();
        thread::yield_now();
        thread::yield_now();

        let candidate_id: Option<String> = {
            let evs = j.events.lock().unwrap();
            let mut started_ids: Vec<String> = Vec::new();
            for (_, ev) in evs.iter() {
                if let Some(id) = ev.tool_call_id() {
                    started_ids.push(id.to_string());
                }
            }
            started_ids.into_iter().last()
        };

        if let Some(tid) = candidate_id {
            let op_id = format!("op_timeout_{tid}");
            j.push(ModelEvent::ToolCallCancelled {
                tool_call_id: tid,
                operation_id: op_id,
                reason: "timeout",
                seq: 0,
            });
        } else {
            let unique = j.next_id();
            let id = format!("tc_timeout_fallback_{unique}");
            let op_id = format!("op_timeout_{unique}");
            let gen_val = j.current_generation.load(Ordering::SeqCst);
            j.push(ModelEvent::ToolCallStarted {
                tool_call_id: id.clone(),
                operation_id: op_id.clone(),
                generation: gen_val,
                seq: 0,
            });
            thread::yield_now();
            j.push(ModelEvent::ToolCallCancelled {
                tool_call_id: id,
                operation_id: op_id,
                reason: "timeout",
                seq: 0,
            });
        }
    }

    fn assert_tool_lifecycle(events: &[ModelEvent]) -> InvariantReportModel {
        #[derive(Debug, Clone)]
        enum LifecycleState {
            Started { seq: u64 },
            Ended { seq: u64, kind: &'static str },
        }

        let mut report = InvariantReportModel::ok();
        let mut states: HashMap<String, LifecycleState> = HashMap::new();

        for ev in events {
            let Some(id) = ev.tool_call_id() else { continue };
            let ev_seq = ev.seq();
            let id_owned = id.to_string();

            let is_start = matches!(ev, ModelEvent::ToolCallStarted { .. });
            let is_end = matches!(
                ev,
                ModelEvent::ToolCallResultCommitted { .. } | ModelEvent::ToolCallCancelled { .. }
            );
            let end_kind = match ev {
                ModelEvent::ToolCallResultCommitted { .. } => Some("committed"),
                ModelEvent::ToolCallCancelled { reason, .. } => Some(*reason),
                _ => None,
            };

            let existing = states.get(&id_owned).cloned();

            if is_start {
                match existing {
                    None => {
                        states.insert(id_owned, LifecycleState::Started { seq: ev_seq });
                    }
                    Some(LifecycleState::Started { seq }) => {
                        report.push(format!(
                            "TOOL_CALL_LIFECYCLE: duplicate ToolCallStarted for id={id}; first seq={seq}, second seq={ev_seq}"
                        ));
                    }
                    Some(LifecycleState::Ended { seq, kind }) => {
                        report.push(format!(
                            "TOOL_CALL_LIFECYCLE: ToolCallStarted for id={id} AFTER it already ended (seq={seq}, kind={kind}); new event seq={ev_seq}"
                        ));
                    }
                }
            } else if is_end {
                let kind = end_kind.unwrap();
                match existing {
                    None => {
                        report.push(format!(
                            "TOOL_CALL_LIFECYCLE: End event (kind={kind}) for id={id} without any prior Started; seq={ev_seq}"
                        ));
                        states.insert(id_owned, LifecycleState::Ended { seq: ev_seq, kind });
                    }
                    Some(LifecycleState::Started { .. }) => {
                        states.insert(id_owned, LifecycleState::Ended { seq: ev_seq, kind });
                    }
                    Some(LifecycleState::Ended { seq, kind: prev_kind }) => {
                        report.push(format!(
                            "TOOL_CALL_LIFECYCLE: End event (kind={kind}) for id={id} AFTER it already ended (seq={seq}, prev_kind={prev_kind}); new event seq={ev_seq}"
                        ));
                    }
                }
            }
        }
        report
    }

    fn assert_gen_monotonic(events: &[ModelEvent]) -> InvariantReportModel {
        let mut report = InvariantReportModel::ok();
        let mut last_seen: Option<(u64, u64)> = None;

        for ev in events {
            let Some(gen_val) = ev.generation() else { continue };
            let ev_seq = ev.seq();
            match last_seen {
                None => {
                    last_seen = Some((gen_val, ev_seq));
                }
                Some((prev_gen, prev_seq)) => {
                    if gen_val < prev_gen {
                        report.push(format!(
                            "GEN_MONOTONIC: generation went backwards: {gen_val} < previous {prev_gen}; violation between seq={prev_seq} and seq={ev_seq}"
                        ));
                    } else {
                        last_seen = Some((gen_val, ev_seq));
                    }
                }
            }
        }
        report
    }

    pub fn run_once<const N: u8>(enable_cancel: bool, enable_revoke: bool, enable_timeout: bool) {
        loom::model(move || {
            let j = SharedJournal::new();

            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let j = Arc::clone(&j);
                    thread::spawn(move || worker_execute_tools(j, N))
                })
                .collect();

            if enable_cancel {
                let j1 = Arc::clone(&j);
                thread::spawn(move || worker_occasionally_cancel(j1));
            }
            if enable_revoke {
                let j1 = Arc::clone(&j);
                thread::spawn(move || worker_revocation_bump(j1));
            }
            if enable_timeout {
                let j1 = Arc::clone(&j);
                thread::spawn(move || worker_timeout(j1));
            }

            for h in handles {
                h.join().unwrap();
            }

            thread::yield_now();
            thread::yield_now();

            let ordered = j.snapshot_sorted();

            let lifecycle_report = assert_tool_lifecycle(&ordered);
            assert!(
                lifecycle_report.all_ok,
                "TOOL_CALL_LIFECYCLE invariant FAILED: {lifecycle_report:?}"
            );

            let gen_report = assert_gen_monotonic(&ordered);
            assert!(
                gen_report.all_ok,
                "GEN_MONOTONIC invariant FAILED: {gen_report:?}"
            );
        });
    }
}

#[cfg(loom)]
#[test]
fn loom_basic_no_cancel_revoke_timeout() {
    model::run_once::<3>(false, false, false);
}

#[cfg(loom)]
#[test]
fn loom_cancel_only() {
    model::run_once::<2>(true, false, false);
}

#[cfg(loom)]
#[test]
fn loom_cancel_timeout_revoke() {
    model::run_once::<2>(true, true, true);
}

#[cfg(loom)]
#[test]
fn loom_revoke_only() {
    model::run_once::<3>(false, true, false);
}
