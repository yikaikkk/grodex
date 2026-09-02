//! Memory Lifecycle Governance — P1-3/P1-4/P1-5.
//!
//! Provides the control plane missing from P0:
//!   1. Conflict detection across active MemoryUnits (§8.2 ConflictsWith edge).
//!   2. Rollout TTL cleanup — when `~/.grodex/sessions/{id}` disappears,
//!      evidence units extracted from that rollout are flagged
//!      `rollout_available = 0` with `rollout_expired_at = now`.
//!   3. Stale / low-signal memory decay: units that haven't been accessed
//!      in > 180 days have their `access_count` decayed so retrieval
//!      rankings trend newer, higher-signal knowledge.
//!   4. Embedding version governance: hooks that compare the current model
//!      id against the persisted active id, drop stale vectors on model
//!      change, and report backfill failure rates.
//!
//! Design constraint: this module NEVER calls an LLM. All decisions come
//! from deterministic heuristics + timestamps + content-hash bucketing
//! (same philosophy as the P0 consolidator) so the governance pass is
//! auditable, reproducible, and safe to run on a schedule.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::database::{DbError, MemoryDatabase};
use crate::types::{EdgeRelation, ProvenanceEdge};

/// Default threshold for "stale" decay.
pub const STALE_ACCESS_DAYS: i64 = 180;
/// Max conflicts detected / decayed units per pass — keeps runtime bounded.
pub const MAX_OPS_PER_PASS: usize = 200;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GovernanceReport {
    pub conflicts_detected: usize,
    pub conflicts_with_edges_created: usize,
    pub rollout_evidences_expired: usize,
    pub stale_memories_decayed: usize,
    pub embedding_model_changed: bool,
    pub embedding_old_rows_deleted: usize,
    pub backfill_batches_failed: usize,
    pub errors: usize,
}

impl MemoryDatabase {
    /// Run a single governance pass:
    ///   - scan conflict candidates (kind + hash-prefix buckets)
    ///   - expire evidence whose rollout dir is gone on disk
    ///   - decay stale memory access counts
    ///   - rotate the active embedding model if `new_model_id` differs
    ///     from the last recorded id (old vectors get dropped so the next
    ///     backfill rebuilds cleanly for the new model).
    ///
    /// Call from a spawn_blocking background task; it never calls the
    /// network. Returns `GovernanceReport` for logging.
    pub fn run_governance_pass(
        &self,
        sessions_root: Option<&std::path::Path>,
        new_model_id: Option<&str>,
    ) -> GovernanceReport {
        let mut report = GovernanceReport::default();

        // ── 1. Conflict candidates ──────────────────────────────────
        match self.list_conflict_candidate_pairs(MAX_OPS_PER_PASS) {
            Ok(pairs) => {
                report.conflicts_detected = pairs.len();
                for (older, newer) in pairs {
                    // Skip if edge already exists in either direction.
                    let already = self
                        .list_relations(&older, &newer)
                        .map(|rels| rels.iter().any(|r| matches!(r, EdgeRelation::ConflictsWith)))
                        .unwrap_or(false)
                        || self
                            .list_relations(&newer, &older)
                            .map(|rels| {
                                rels.iter().any(|r| matches!(r, EdgeRelation::ConflictsWith))
                            })
                            .unwrap_or(false);
                    if already {
                        continue;
                    }
                    // The memory_evidence_edges PK spans (memory_id,
                    // evidence_id, relation). ConflictsWith tracks a
                    // memory↔memory relationship; we stash the OTHER id
                    // in `evidence_id`. This is semantically a bit of a
                    // stretch, but it reuses the existing FK-free edge
                    // table without a schema change (see P2 roadmap for
                    // dedicated memory↔memory edges).
                    if self.insert_conflicts_with_edge(&older, &newer).is_ok() {
                        report.conflicts_with_edges_created += 1;
                    } else {
                        report.errors += 1;
                    }
                }
            }
            Err(_) => report.errors += 1,
        }

        // ── 2. Rollout TTL expiry ───────────────────────────────────
        if let Some(root) = sessions_root {
            match self.list_rollout_missing_evidences(root) {
                Ok(ids) => {
                    for id in ids.iter().take(MAX_OPS_PER_PASS) {
                        if self.mark_rollout_expired(id).is_ok() {
                            report.rollout_evidences_expired += 1;
                        } else {
                            report.errors += 1;
                        }
                    }
                }
                Err(_) => report.errors += 1,
            }
        }

        // ── 3. Stale memory decay ───────────────────────────────────
        match self.decay_stale_memories(STALE_ACCESS_DAYS, MAX_OPS_PER_PASS) {
            Ok(n) => report.stale_memories_decayed = n,
            Err(_) => report.errors += 1,
        }

        // ── 4. Embedding model rotation ─────────────────────────────
        if let Some(mid) = new_model_id {
            match self.set_active_embedding_model(mid) {
                Ok((changed, deleted)) => {
                    report.embedding_model_changed = changed;
                    report.embedding_old_rows_deleted = deleted;
                }
                Err(_) => report.errors += 1,
            }
        }

        report
    }

    /// Apply access-count decay to memory units that haven't been accessed
    /// in `threshold_days`. Returns the number of rows updated.
    ///
    /// Rationale: we don't delete units automatically (the user owns the
    /// Markdown source). Instead, `access_count` is halved once per
    /// governance pass so BM25-blind ranking strategies (P2) can layer a
    /// recency / provenance-quality multiplier on top. Units with
    /// `last_accessed_at IS NULL` (never retrieved) fall back to
    /// `updated_at` + threshold for the staleness check.
    pub fn decay_stale_memories(
        &self,
        threshold_days: i64,
        limit: usize,
    ) -> Result<usize, DbError> {
        let cutoff: DateTime<Utc> = Utc::now()
            .checked_sub_signed(Duration::days(threshold_days))
            .unwrap_or_else(|| Utc::now());
        let cutoff_s = cutoff.to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE memory_units SET access_count = MAX(0, access_count / 2), last_accessed_at = ?1
             WHERE id IN (
               SELECT id FROM memory_units
               WHERE status = 'active' AND access_count > 0 AND (
                 (last_accessed_at IS NOT NULL AND last_accessed_at < ?2)
                 OR
                 (last_accessed_at IS NULL AND updated_at < ?2)
               )
               LIMIT ?3
             )",
            rusqlite::params![Utc::now().to_rfc3339(), cutoff_s, limit as i64],
        )?;
        Ok(changed)
    }
}

/// Report builder: convert a [`GovernanceReport`] into a short, human-readable
/// banner suitable for startup stderr / logs.
pub fn format_governance_banner(rpt: &GovernanceReport) -> String {
    let mut parts: Vec<String> = Vec::new();
    if rpt.conflicts_with_edges_created > 0 {
        parts.push(format!(
            "conflicts: +{} edges ({} candidates)",
            rpt.conflicts_with_edges_created, rpt.conflicts_detected
        ));
    }
    if rpt.rollout_evidences_expired > 0 {
        parts.push(format!("expired {} orphan rollout evidences", rpt.rollout_evidences_expired));
    }
    if rpt.stale_memories_decayed > 0 {
        parts.push(format!("decayed {} stale memories", rpt.stale_memories_decayed));
    }
    if rpt.embedding_model_changed {
        parts.push(format!(
            "embedding model rotated; deleted {} old vector rows",
            rpt.embedding_old_rows_deleted
        ));
    }
    if rpt.errors > 0 {
        parts.push(format!("{} non-fatal errors", rpt.errors));
    }
    if parts.is_empty() {
        String::from("governance pass: no changes")
    } else {
        format!("governance: {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::apply_schema;
    use crate::types::{
        EvidenceStatus, EvidenceUnit, MemoryKind, MemoryScope, MemoryUnit, UnitStatus,
    };
    use rusqlite::Connection;
    use sha2::{Digest, Sha256};

    fn make_db() -> MemoryDatabase {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        MemoryDatabase::from_conn(conn)
    }

    fn insert_memory(
        db: &MemoryDatabase,
        id: &str,
        content: &str,
        kind: MemoryKind,
    ) -> MemoryUnit {
        let mut h = Sha256::new();
        h.update(content.as_bytes());
        let content_hash = format!("{:x}", h.finalize());
        let now = Utc::now();
        let mu = MemoryUnit {
            id: id.into(),
            path: "MEMORY.md".into(),
            section: String::new(),
            kind,
            scope: MemoryScope::Workspace,
            status: UnitStatus::Active,
            content: content.into(),
            content_hash,
            updated_at: now,
            created_at: now,
        };
        db.upsert_memory_unit(&mu).unwrap();
        mu
    }

    fn insert_active_evidence(
        db: &MemoryDatabase,
        id: &str,
        rollout_id: &str,
        content: &str,
    ) -> EvidenceUnit {
        let mut h = Sha256::new();
        h.update(content.as_bytes());
        let content_hash = format!("{:x}", h.finalize());
        let now = Utc::now();
        let eu = EvidenceUnit {
            id: id.into(),
            rollout_id: rollout_id.into(),
            path: "__test__".into(),
            section: String::new(),
            scope: MemoryScope::Workspace,
            status: EvidenceStatus::Active,
            content: content.into(),
            content_hash,
            occurred_at: now,
            created_at: now,
            superseded_by: None,
            superseded_at: None,
            rollout_available: true,
            rollout_expired_at: None,
            subchunk_index: 0,
        };
        db.upsert_evidence_unit(&eu).unwrap();
        eu
    }

    #[test]
    fn governance_detects_conflicts_for_same_kind_hash_prefix() {
        let db = make_db();
        // Same content → identical hash → same 12-char prefix → candidate pair.
        insert_memory(&db, "mem_a", "openssl must be linked statically", MemoryKind::Constraint);
        insert_memory(&db, "mem_b", "openssl must be linked statically", MemoryKind::Constraint);

        let rpt = db.run_governance_pass(None, None);
        assert_eq!(rpt.conflicts_detected, 1);
        assert_eq!(rpt.conflicts_with_edges_created, 1);
        // Idempotence: second pass adds 0 new edges.
        let r2 = db.run_governance_pass(None, None);
        assert_eq!(r2.conflicts_with_edges_created, 0);
    }

    #[test]
    fn governance_expires_evidence_for_missing_rollout_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = make_db();
        // Session dir exists.
        let existing_session = tmp.path().join("sess_exist");
        std::fs::create_dir_all(&existing_session).unwrap();
        std::fs::write(existing_session.join("rollout.jsonl"), b"{}").unwrap();
        insert_active_evidence(&db, "ev_exists", "sess_exist", "still on disk");
        // Session dir absent.
        insert_active_evidence(&db, "ev_missing", "sess_gone", "directory deleted");

        let rpt = db.run_governance_pass(Some(tmp.path()), None);
        assert_eq!(rpt.rollout_evidences_expired, 1);
        let ev = db.get_evidence_unit("ev_missing").unwrap().unwrap();
        assert!(!ev.rollout_available);
        assert!(ev.rollout_expired_at.is_some());
        // ev_exists is still available.
        let ev2 = db.get_evidence_unit("ev_exists").unwrap().unwrap();
        assert!(ev2.rollout_available);
    }

    #[test]
    fn governance_decays_stale_memories() {
        let db = make_db();
        let mu = insert_memory(&db, "mem_stale", "stale content", MemoryKind::Fact);
        // Artificially set access_count + old last_accessed_at via raw SQL.
        {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            drop(conn); // just a compile safety check; we use the db below
        }
        {
            let conn = db.conn.lock().unwrap();
            let old = Utc::now()
                .checked_sub_signed(Duration::days(365))
                .unwrap()
                .to_rfc3339();
            conn.execute(
                "UPDATE memory_units SET access_count = 10, last_accessed_at = ?1 WHERE id = ?2",
                rusqlite::params![old, mu.id],
            )
            .unwrap();
        }
        let rpt = db.run_governance_pass(None, None);
        assert!(rpt.stale_memories_decayed >= 1);
        let got = db.get_memory_unit("mem_stale").unwrap().unwrap();
        // access_count(10) / 2 == 5
        {
            let conn = db.conn.lock().unwrap();
            let n: i64 = conn
                .query_row(
                    "SELECT access_count FROM memory_units WHERE id = ?1",
                    rusqlite::params![mu.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 5);
        }
    }

    #[test]
    fn embedding_model_rotation_drops_old_vectors() {
        let db = make_db();
        let _ = insert_memory(&db, "m1", "hello world", MemoryKind::Fact);
        // Write a row for the "old" model directly.
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO document_embeddings
                   (doc_ref, chunk_index, embedding_model, embedding_dim, vector_blob, created_at_ms)
                 VALUES ('mem:m1', 0, 'old-model', 2, x'0000000000000000', 0)",
                [],
            )
            .unwrap();
        }
        // 1) Register old-model as the active id — no previous id, so no
        //    deletions yet.
        let r1 = db.set_active_embedding_model("old-model").unwrap();
        assert!(r1.0);  // changed
        assert_eq!(r1.1, 0);
        // 2) Rotate to new-model → previous = "old-model" → DELETE old rows.
        let rpt = db.run_governance_pass(None, Some("new-model"));
        assert!(rpt.embedding_model_changed);
        assert_eq!(rpt.embedding_old_rows_deleted, 1);
        // Subsequent run with same new model is a no-op.
        let r2 = db.run_governance_pass(None, Some("new-model"));
        assert!(!r2.embedding_model_changed);
    }
}
