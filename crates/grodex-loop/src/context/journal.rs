//! Compaction journal (doc 11 §18).
//!
//! Tracks the lifecycle of every compaction so that a crashed session can
//! recover deterministically:
//!   §18.1 Protocol — Started → CandidateBuilt → Committed (or Failed).
//!   §18.2 Crash recovery — only a Committed entry installs a candidate.
//!   §18.3 Idempotency — Committed uses a stable compaction_id; same hash
//!          replays are ignored, different hashes flag journal corruption.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A compaction journal entry (doc 11 §18.1). Tracks the lifecycle of a
/// single compaction operation for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionJournalEntry {
    pub compaction_id: String,
    pub seq: u64,
    pub stage: CompactionStage,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStage {
    /// §18.1 step 1: compaction started with a plan.
    Started { plan_summary: String },
    /// §18.1 step 3: candidate built, awaiting verification.
    CandidateBuilt { candidate_ref: String, candidate_hash: String },
    /// §18.1 step 5: committed — replacement history installed.
    Committed {
        replacement_history_hash: String,
        state_capsule_hash: String,
        stable_prefix_hash: String,
        history_version: u64,
    },
    /// Compaction failed and was rolled back.
    Failed { reason: String },
}

impl CompactionStage {
    /// Whether this stage represents a terminal, committed state.
    pub fn is_committed(&self) -> bool {
        matches!(self, CompactionStage::Committed { .. })
    }
}

/// The CompactionJournal tracks all compaction operations for crash
/// recovery (doc 11 §18). On restart, the journal tells us whether
/// the last compaction was committed or interrupted.
#[derive(Debug, Clone, Default)]
pub struct CompactionJournal {
    entries: Vec<CompactionJournalEntry>,
}

/// Result of crash recovery analysis (doc 11 §18.2).
#[derive(Debug, Clone)]
pub enum RecoveryAction {
    /// Last compaction was committed; install this replacement history.
    Install {
        compaction_id: String,
        history_version: u64,
        replacement_history_hash: String,
    },
    /// Compaction was interrupted (Started or CandidateBuilt but not Committed),
    /// or it Failed. Do NOT install the candidate; continue with the previous
    /// checkpoint.
    Interrupted {
        compaction_id: String,
        last_stage: CompactionStage,
    },
    /// No compaction history found.
    NoHistory,
    /// Journal corruption detected (hash mismatch on replay).
    Corrupted {
        compaction_id: String,
        expected_hash: String,
        actual_hash: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyCheck {
    /// Same compaction_id + same hash → safe to ignore (idempotent replay).
    DuplicateSafe,
    /// Same compaction_id but different hash → journal corruption.
    CorruptionDetected,
    /// New compaction_id → proceed normally.
    New,
}

impl CompactionJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of recorded entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Append a journal entry. Callers are responsible for assigning a
    /// monotonically increasing `seq`.
    pub fn append(&mut self, entry: CompactionJournalEntry) {
        self.entries.push(entry);
    }

    /// Get all entries for a specific compaction_id, in append order.
    pub fn entries_for(&self, compaction_id: &str) -> Vec<&CompactionJournalEntry> {
        self.entries
            .iter()
            .filter(|e| e.compaction_id == compaction_id)
            .collect()
    }

    /// Get the last committed compaction entry, if any.
    pub fn last_committed(&self) -> Option<&CompactionJournalEntry> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.stage.is_committed())
    }

    /// §18.2 Crash recovery: analyze the journal to determine what to
    /// install on restart.
    ///
    /// Looks at the most recent compaction (the one with the highest-seq
    /// entry). If its final stage is `Committed`, the replacement history
    /// should be installed. Otherwise the compaction was interrupted and
    /// the previous checkpoint must be kept.
    pub fn analyze_for_recovery(&self) -> RecoveryAction {
        let last = match self.entries.last() {
            Some(e) => e,
            None => return RecoveryAction::NoHistory,
        };
        let compaction_id = &last.compaction_id;
        let stages = self.entries_for(compaction_id);
        // entries_for preserves append order, so the last one is the
        // latest stage for this compaction.
        let last_stage = stages
            .last()
            .map(|e| e.stage.clone())
            .expect("at least one entry exists for the last compaction_id");

        match &last_stage {
            CompactionStage::Committed {
                replacement_history_hash,
                history_version,
                ..
            } => RecoveryAction::Install {
                compaction_id: compaction_id.clone(),
                history_version: *history_version,
                replacement_history_hash: replacement_history_hash.clone(),
            },
            _ => RecoveryAction::Interrupted {
                compaction_id: compaction_id.clone(),
                last_stage,
            },
        }
    }

    /// §18.3 Idempotency: check whether a committed compaction_id already
    /// exists with the same hash (ignore) or a different hash (corruption).
    pub fn check_idempotency(
        &self,
        compaction_id: &str,
        replacement_history_hash: &str,
    ) -> IdempotencyCheck {
        for entry in self.entries_for(compaction_id) {
            if let CompactionStage::Committed {
                replacement_history_hash: existing_hash,
                ..
            } = &entry.stage
            {
                if existing_hash == replacement_history_hash {
                    return IdempotencyCheck::DuplicateSafe;
                } else {
                    return IdempotencyCheck::CorruptionDetected;
                }
            }
        }
        IdempotencyCheck::New
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn started(id: &str, seq: u64) -> CompactionJournalEntry {
        CompactionJournalEntry {
            compaction_id: id.to_string(),
            seq,
            stage: CompactionStage::Started {
                plan_summary: "plan".to_string(),
            },
            timestamp: now(),
        }
    }

    fn candidate(id: &str, seq: u64, hash: &str) -> CompactionJournalEntry {
        CompactionJournalEntry {
            compaction_id: id.to_string(),
            seq,
            stage: CompactionStage::CandidateBuilt {
                candidate_ref: format!("ref-{id}"),
                candidate_hash: hash.to_string(),
            },
            timestamp: now(),
        }
    }

    fn committed(id: &str, seq: u64, hash: &str, version: u64) -> CompactionJournalEntry {
        CompactionJournalEntry {
            compaction_id: id.to_string(),
            seq,
            stage: CompactionStage::Committed {
                replacement_history_hash: hash.to_string(),
                state_capsule_hash: "capsule".to_string(),
                stable_prefix_hash: "prefix".to_string(),
                history_version: version,
            },
            timestamp: now(),
        }
    }

    fn failed(id: &str, seq: u64, reason: &str) -> CompactionJournalEntry {
        CompactionJournalEntry {
            compaction_id: id.to_string(),
            seq,
            stage: CompactionStage::Failed {
                reason: reason.to_string(),
            },
            timestamp: now(),
        }
    }

    #[test]
    fn empty_journal_recovers_no_history() {
        let journal = CompactionJournal::new();
        assert!(matches!(journal.analyze_for_recovery(), RecoveryAction::NoHistory));
        assert!(journal.is_empty());
    }

    #[test]
    fn committed_sequence_recovers_install() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(candidate("c1", 2, "h-cand"));
        journal.append(committed("c1", 3, "h-repl", 5));

        match journal.analyze_for_recovery() {
            RecoveryAction::Install {
                compaction_id,
                history_version,
                replacement_history_hash,
            } => {
                assert_eq!(compaction_id, "c1");
                assert_eq!(history_version, 5);
                assert_eq!(replacement_history_hash, "h-repl");
            }
            other => panic!("expected Install, got {other:?}"),
        }
    }

    #[test]
    fn started_but_not_committed_is_interrupted() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        match journal.analyze_for_recovery() {
            RecoveryAction::Interrupted { compaction_id, last_stage } => {
                assert_eq!(compaction_id, "c1");
                assert!(matches!(last_stage, CompactionStage::Started { .. }));
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }
    }

    #[test]
    fn candidate_but_not_committed_is_interrupted() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(candidate("c1", 2, "h-cand"));
        match journal.analyze_for_recovery() {
            RecoveryAction::Interrupted { compaction_id, last_stage } => {
                assert_eq!(compaction_id, "c1");
                assert!(matches!(last_stage, CompactionStage::CandidateBuilt { .. }));
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }
    }

    #[test]
    fn failed_compaction_is_interrupted() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(candidate("c1", 2, "h-cand"));
        journal.append(failed("c1", 3, "verifier rejected"));
        match journal.analyze_for_recovery() {
            RecoveryAction::Interrupted { last_stage, .. } => {
                assert!(matches!(last_stage, CompactionStage::Failed { .. }));
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }
    }

    #[test]
    fn latest_committed_wins_when_multiple_compactions() {
        let mut journal = CompactionJournal::new();
        // first compaction fully committed
        journal.append(started("c1", 1));
        journal.append(committed("c1", 2, "h1", 1));
        // second compaction committed later
        journal.append(started("c2", 3));
        journal.append(committed("c2", 4, "h2", 2));

        match journal.analyze_for_recovery() {
            RecoveryAction::Install {
                compaction_id,
                history_version,
                replacement_history_hash,
            } => {
                assert_eq!(compaction_id, "c2");
                assert_eq!(history_version, 2);
                assert_eq!(replacement_history_hash, "h2");
            }
            other => panic!("expected Install for c2, got {other:?}"),
        }
    }

    #[test]
    fn interrupted_second_compaction_keeps_first_committed() {
        // Even though c1 committed, the latest compaction c2 was
        // interrupted → recovery returns Interrupted (caller keeps the
        // already-installed c1 checkpoint rather than installing c2).
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(committed("c1", 2, "h1", 1));
        journal.append(started("c2", 3));
        journal.append(candidate("c2", 4, "h2-cand"));

        match journal.analyze_for_recovery() {
            RecoveryAction::Interrupted { compaction_id, .. } => {
                assert_eq!(compaction_id, "c2");
            }
            other => panic!("expected Interrupted for c2, got {other:?}"),
        }
    }

    #[test]
    fn idempotency_same_id_same_hash_is_duplicate_safe() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(committed("c1", 2, "h-repl", 5));

        assert_eq!(
            journal.check_idempotency("c1", "h-repl"),
            IdempotencyCheck::DuplicateSafe
        );
    }

    #[test]
    fn idempotency_same_id_different_hash_is_corruption() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(committed("c1", 2, "h-repl", 5));

        assert_eq!(
            journal.check_idempotency("c1", "h-different"),
            IdempotencyCheck::CorruptionDetected
        );
    }

    #[test]
    fn idempotency_new_id_is_new() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(committed("c1", 2, "h-repl", 5));

        assert_eq!(
            journal.check_idempotency("c2", "h-repl"),
            IdempotencyCheck::New
        );
    }

    #[test]
    fn idempotency_ignores_uncommitted_entries() {
        // A compaction_id that only has Started/CandidateBuilt entries
        // is not a committed duplicate → New.
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(candidate("c1", 2, "h-cand"));

        assert_eq!(
            journal.check_idempotency("c1", "h-cand"),
            IdempotencyCheck::New
        );
    }

    #[test]
    fn last_committed_finds_latest() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(committed("c1", 2, "h1", 1));
        journal.append(started("c2", 3));

        let last = journal.last_committed().unwrap();
        assert_eq!(last.compaction_id, "c1");
        assert_eq!(last.seq, 2);
    }

    #[test]
    fn last_committed_none_when_no_commit() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(candidate("c1", 2, "h-cand"));
        assert!(journal.last_committed().is_none());
    }

    #[test]
    fn entries_for_returns_in_order() {
        let mut journal = CompactionJournal::new();
        journal.append(started("c1", 1));
        journal.append(started("c2", 2));
        journal.append(candidate("c1", 3, "h-cand"));

        let c1 = journal.entries_for("c1");
        assert_eq!(c1.len(), 2);
        assert_eq!(c1[0].seq, 1);
        assert_eq!(c1[1].seq, 3);

        let c2 = journal.entries_for("c2");
        assert_eq!(c2.len(), 1);
        assert_eq!(c2[0].seq, 2);

        assert!(journal.entries_for("c3").is_empty());
    }

    #[test]
    fn journal_entry_serializes_round_trip() {
        let entry = committed("c1", 1, "h", 3);
        let json = serde_json::to_string(&entry).unwrap();
        let back: CompactionJournalEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.compaction_id, "c1");
        assert_eq!(back.seq, 1);
        match back.stage {
            CompactionStage::Committed {
                history_version, ..
            } => assert_eq!(history_version, 3),
            _ => panic!("wrong stage"),
        }
    }

    #[test]
    fn stage_serializes_snake_case() {
        let stage = CompactionStage::CandidateBuilt {
            candidate_ref: "r".into(),
            candidate_hash: "h".into(),
        };
        let json = serde_json::to_string(&stage).unwrap();
        assert!(json.contains("\"candidate_built\""));
        let back: CompactionStage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, stage);
    }
}
