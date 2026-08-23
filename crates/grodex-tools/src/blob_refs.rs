//! `blob_refs` projection + grace-period GC (Doc 11 §22, Phase 2).
//!
//! Blob liveness must never be guessed from directory scans; it is managed
//! by a REBUILDABLE reference projection:
//!
//! ```text
//! blob_refs(blob_hash, owner_kind, owner_id, ref_kind, created_seq, expires_at)
//! ```
//!
//! Candidate compactions, committed checkpoints, Tool Results, Memory
//! Evidence and Sessions each register their own references. When a
//! candidate is voided or a Session/Memory is deleted, ONLY that owner's
//! references are removed. A blob may be GC'd only when:
//!
//! 1. its reference count has dropped to zero, AND
//! 2. it has stayed at zero for at least the retention grace period
//!    (and any explicit per-ref `expires_at` has passed).
//!
//! The ledger is a PROJECTION, not a new source of truth: it can be fully
//! rebuilt from rollout, Memory manifests and checkpoints
//! ([`BlobRefLedger::rebuild`]), so losing it costs nothing.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

/// Who holds a reference to a blob (Doc 11 §22 owner list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BlobOwnerKind {
    /// A compaction candidate that has not been committed yet (voided
    /// candidates drop their refs).
    CandidateCompaction,
    /// A committed checkpoint.
    Checkpoint,
    /// A large Tool Result offloaded to the blob store.
    ToolResult,
    /// A Memory evidence entry.
    MemoryEvidence,
    /// The session itself (cascade-delete removes these).
    Session,
}

/// Why the reference exists (finer-grained than the owner kind; kept for
/// parity with the Doc 11 schema column `ref_kind`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobRefKind {
    /// The blob is the candidate's replacement-history payload.
    CompactionPayload,
    /// The blob is a checkpoint's serialized history.
    CheckpointPayload,
    /// The blob is a tool output body.
    ToolOutputBody,
    /// The blob backs a memory evidence citation.
    EvidenceCitation,
    /// Session-scoped attachment.
    SessionAttachment,
}

/// One row of the `blob_refs` projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRefRecord {
    pub blob_hash: String,
    pub owner_kind: BlobOwnerKind,
    pub owner_id: String,
    pub ref_kind: BlobRefKind,
    /// Monotonic journal sequence at registration time (audit order).
    pub created_seq: u64,
    /// Optional explicit expiry — a blob cannot be GC'd before every one
    /// of its refs' `expires_at` has passed (TTL per Doc 11 §22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<SystemTime>,
}

/// Per-blob live state inside the projection.
#[derive(Debug, Clone)]
struct BlobState {
    refs: Vec<BlobRefRecord>,
    /// When the ref count last dropped to (or started at) zero; the grace
    /// period is measured from this instant.
    zero_since: SystemTime,
    /// Latest explicit TTL seen across this blob's refs (Doc 11 §22: blob
    /// TTL). Deliberately RETAINED after the ref carrying it is removed —
    /// the TTL is a property of the blob, not of the ref.
    max_expires_at: Option<SystemTime>,
}

/// The rebuildable `blob_refs` projection.
#[derive(Debug, Clone)]
pub struct BlobRefLedger {
    state: BTreeMap<String, BlobState>,
    next_seq: u64,
}

impl Default for BlobRefLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl BlobRefLedger {
    pub fn new() -> Self {
        Self { state: BTreeMap::new(), next_seq: 0 }
    }

    /// Highest sequence handed out so far (callers rebuilding incrementally
    /// should start after it).
    pub fn last_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// Register a reference. Identical `(blob_hash, owner_kind, owner_id)`
    /// tuples are idempotent — re-registering returns the existing record.
    pub fn add_ref(
        &mut self,
        blob_hash: impl Into<String>,
        owner_kind: BlobOwnerKind,
        owner_id: impl Into<String>,
        ref_kind: BlobRefKind,
        now: SystemTime,
        expires_at: Option<SystemTime>,
    ) -> BlobRefRecord {
        let blob_hash = blob_hash.into();
        let owner_id = owner_id.into();
        let entry = self.state.entry(blob_hash.clone()).or_insert_with(|| BlobState {
            refs: Vec::new(),
            zero_since: now,
            max_expires_at: None,
        });
        if let Some(existing) = entry
            .refs
            .iter()
            .find(|r| r.owner_kind == owner_kind && r.owner_id == owner_id)
        {
            return existing.clone();
        }
        if let Some(exp) = expires_at {
            entry.max_expires_at = Some(entry.max_expires_at.map_or(exp, |m| m.max(exp)));
        }
        let record = BlobRefRecord {
            blob_hash,
            owner_kind,
            owner_id,
            ref_kind,
            created_seq: self.next_seq,
            expires_at,
        };
        self.next_seq += 1;
        entry.refs.push(record.clone());
        record
    }

    /// Remove ONLY the references owned by `(owner_kind, owner_id)` —
    /// candidate void / Session delete / Memory delete semantics
    /// (Doc 11 §22: never touch other owners' refs). Returns the blob
    /// hashes whose refs were removed.
    pub fn remove_owner_refs(
        &mut self,
        owner_kind: BlobOwnerKind,
        owner_id: &str,
        now: SystemTime,
    ) -> Vec<String> {
        let mut touched = Vec::new();
        for (hash, st) in self.state.iter_mut() {
            let before = st.refs.len();
            st.refs.retain(|r| !(r.owner_kind == owner_kind && r.owner_id == owner_id));
            if st.refs.len() != before {
                if st.refs.is_empty() {
                    // Grace period starts the moment the count hits zero.
                    st.zero_since = now;
                }
                touched.push(hash.clone());
            }
        }
        touched
    }

    /// Current live reference count for a blob (0 if unknown).
    pub fn ref_count(&self, blob_hash: &str) -> usize {
        self.state.get(blob_hash).map(|s| s.refs.len()).unwrap_or(0)
    }

    /// All blob hashes currently tracked (any ref count).
    pub fn tracked_blobs(&self) -> BTreeSet<String> {
        self.state.keys().cloned().collect()
    }

    /// Blobs eligible for GC at `now`:
    /// ref count == 0 AND zero for >= `grace` AND every explicit TTL
    /// (`expires_at`) ever recorded on the blob has passed — the TTL
    /// outlives the ref that carried it.
    pub fn collect_garbage(&self, now: SystemTime, grace: Duration) -> Vec<String> {
        self.state
            .iter()
            .filter(|(_, st)| st.refs.is_empty())
            .filter(|(_, st)| {
                now.duration_since(st.zero_since).map(|age| age >= grace).unwrap_or(true)
            })
            .filter(|(_, st)| st.max_expires_at.map_or(true, |exp| now >= exp))
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Purge GC'd blobs from the projection after the store deleted them.
    /// Returns the number of entries forgotten.
    pub fn forget(&mut self, blob_hashes: &[String]) -> usize {
        let mut n = 0;
        for h in blob_hashes {
            if self.state.remove(h).is_some() {
                n += 1;
            }
        }
        n
    }

    /// Rebuild the projection from source-of-truth records (rollout,
    /// Memory manifest, checkpoints). The ledger is never authoritative —
    /// this replaces ALL current state and resets grace clocks to `now`
    /// (conservative: a rebuilt zero-ref blob must still survive a full
    /// grace period before GC).
    pub fn rebuild(&mut self, sources: &[BlobRefRecord], now: SystemTime) {
        self.state.clear();
        let mut max_seq: u64 = 0;
        for record in sources {
            let entry = self.state.entry(record.blob_hash.clone()).or_insert_with(|| BlobState {
                refs: Vec::new(),
                zero_since: now,
                max_expires_at: record.expires_at,
            });
            if let Some(exp) = record.expires_at {
                entry.max_expires_at = Some(entry.max_expires_at.map_or(exp, |m| m.max(exp)));
            }
            let dup = entry
                .refs
                .iter()
                .any(|r| r.owner_kind == record.owner_kind && r.owner_id == record.owner_id);
            if !dup {
                entry.refs.push(record.clone());
            }
            max_seq = max_seq.max(record.created_seq);
        }
        self.next_seq = max_seq + 1;
    }

    /// Export the live projection rows (for persistence / diffing against
    /// a rebuild — drift detection).
    pub fn records(&self) -> Vec<BlobRefRecord> {
        self.state.values().flat_map(|st| st.refs.iter().cloned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn ref_count_tracks_multiple_owners() {
        let mut ledger = BlobRefLedger::new();
        ledger.add_ref("h1", BlobOwnerKind::ToolResult, "call-1", BlobRefKind::ToolOutputBody, t(1), None);
        ledger.add_ref("h1", BlobOwnerKind::Checkpoint, "cp-7", BlobRefKind::CheckpointPayload, t(2), None);
        ledger.add_ref("h2", BlobOwnerKind::Session, "s-1", BlobRefKind::SessionAttachment, t(3), None);
        assert_eq!(ledger.ref_count("h1"), 2);
        assert_eq!(ledger.ref_count("h2"), 1);
        assert_eq!(ledger.ref_count("missing"), 0);
    }

    #[test]
    fn duplicate_owner_ref_is_idempotent() {
        let mut ledger = BlobRefLedger::new();
        let a = ledger.add_ref("h1", BlobOwnerKind::Session, "s-1", BlobRefKind::SessionAttachment, t(1), None);
        let b = ledger.add_ref("h1", BlobOwnerKind::Session, "s-1", BlobRefKind::SessionAttachment, t(9), None);
        assert_eq!(a, b);
        assert_eq!(ledger.ref_count("h1"), 1);
    }

    #[test]
    fn removing_one_owner_never_touches_others() {
        let mut ledger = BlobRefLedger::new();
        ledger.add_ref("h1", BlobOwnerKind::CandidateCompaction, "cand-1", BlobRefKind::CompactionPayload, t(1), None);
        ledger.add_ref("h1", BlobOwnerKind::Checkpoint, "cp-1", BlobRefKind::CheckpointPayload, t(2), None);
        // Candidate voided → only its ref goes away.
        let touched = ledger.remove_owner_refs(BlobOwnerKind::CandidateCompaction, "cand-1", t(3));
        assert_eq!(touched, vec!["h1".to_string()]);
        assert_eq!(ledger.ref_count("h1"), 1);
        // Not eligible: checkpoint still references it.
        assert!(ledger.collect_garbage(t(3) + Duration::from_secs(999), Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn zero_ref_blob_must_survive_the_grace_period() {
        let mut ledger = BlobRefLedger::new();
        ledger.add_ref("h1", BlobOwnerKind::ToolResult, "call-1", BlobRefKind::ToolOutputBody, t(10), None);
        ledger.remove_owner_refs(BlobOwnerKind::ToolResult, "call-1", t(100));
        let grace = Duration::from_secs(60);
        // 30s after zeroing: still protected.
        assert!(ledger.collect_garbage(t(130), grace).is_empty());
        // Exactly at grace expiry: eligible.
        assert_eq!(ledger.collect_garbage(t(160), grace), vec!["h1".to_string()]);
    }

    #[test]
    fn explicit_expires_at_blocks_early_gc_even_after_ref_removal() {
        let mut ledger = BlobRefLedger::new();
        ledger.add_ref(
            "h1",
            BlobOwnerKind::ToolResult,
            "call-1",
            BlobRefKind::ToolOutputBody,
            t(10),
            Some(t(500)),
        );
        ledger.remove_owner_refs(BlobOwnerKind::ToolResult, "call-1", t(100));
        // Grace elapsed (t(200) >= t(100)+60) but the blob's TTL (t(500))
        // still holds — the TTL outlives the ref that carried it.
        assert!(ledger.collect_garbage(t(200), Duration::from_secs(60)).is_empty());
        // After the TTL: eligible.
        assert_eq!(
            ledger.collect_garbage(t(500), Duration::from_secs(60)),
            vec!["h1".to_string()]
        );
    }

    #[test]
    fn rebuild_replaces_state_and_resets_grace_clocks() {
        let mut ledger = BlobRefLedger::new();
        ledger.add_ref("old", BlobOwnerKind::Session, "s-1", BlobRefKind::SessionAttachment, t(1), None);
        // Rebuild from source-of-truth records: only h1 exists, zero refs.
        let sources = vec![BlobRefRecord {
            blob_hash: "h1".into(),
            owner_kind: BlobOwnerKind::MemoryEvidence,
            owner_id: "mem-9".into(),
            ref_kind: BlobRefKind::EvidenceCitation,
            created_seq: 41,
            expires_at: None,
        }];
        ledger.rebuild(&sources, t(1000));
        assert_eq!(ledger.ref_count("old"), 0);
        assert!(!ledger.tracked_blobs().contains("old"));
        assert_eq!(ledger.ref_count("h1"), 1);
        assert_eq!(ledger.last_seq(), 41);
        // Remove the ref: grace clock restarted at rebuild+removal time —
        // not retroactively eligible.
        ledger.remove_owner_refs(BlobOwnerKind::MemoryEvidence, "mem-9", t(1000));
        assert!(ledger.collect_garbage(t(1030), Duration::from_secs(60)).is_empty());
        assert_eq!(ledger.collect_garbage(t(1060), Duration::from_secs(60)), vec!["h1".to_string()]);
    }

    #[test]
    fn forget_purges_gc_blobs_from_projection() {
        let mut ledger = BlobRefLedger::new();
        ledger.add_ref("h1", BlobOwnerKind::ToolResult, "c", BlobRefKind::ToolOutputBody, t(1), None);
        ledger.remove_owner_refs(BlobOwnerKind::ToolResult, "c", t(2));
        let gc = ledger.collect_garbage(t(200), Duration::from_secs(60));
        assert_eq!(ledger.forget(&gc), 1);
        assert!(ledger.tracked_blobs().is_empty());
        assert_eq!(ledger.forget(&gc), 0);
    }

    #[test]
    fn records_round_trip_via_serde() {
        let mut ledger = BlobRefLedger::new();
        ledger.add_ref("h1", BlobOwnerKind::Checkpoint, "cp-1", BlobRefKind::CheckpointPayload, t(1), Some(t(99)));
        let recs = ledger.records();
        let json = serde_json::to_string(&recs).unwrap();
        let back: Vec<BlobRefRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].expires_at, Some(t(99)));
        // Rebuild from the round-tripped records yields identical counts.
        let mut rebuilt = BlobRefLedger::new();
        rebuilt.rebuild(&back, t(50));
        assert_eq!(rebuilt.ref_count("h1"), 1);
    }
}
