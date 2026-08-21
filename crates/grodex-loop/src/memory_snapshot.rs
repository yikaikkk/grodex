//! MemoryContextSnapshot — frozen memory retrieval state for a Turn.
//!
//! Invariant #10: MemoryContextSnapshot is stable within a Turn.
//!
//! When a Turn starts, the supervisor performs memory retrieval (FTS5
//! and optionally vector). The results are frozen into a
//! `MemoryContextSnapshot` keyed by `(turn_id, index_generation,
//! query_fingerprint)`. Any subsequent memory query within the same
//! Turn that matches the same fingerprint returns the cached result
//! rather than re-querying the database — this prevents drift if the
//! underlying files change mid-Turn.
//!
//! The snapshot is invalidated when:
//!   - A new Turn starts (new `turn_id`)
//!   - The index generation changes (file was added/removed/reindexed)
//!   - The query fingerprint differs (different user intent)

use grodex_core::id::TurnId;
use grodex_memory::router::QueryFingerprint;
use grodex_memory::retrievers::CombinedRetrieval;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A frozen snapshot of memory retrieval results for a single Turn.
///
/// Created at Turn start after the initial memory retrieval completes.
/// Subsequent queries within the same Turn consult this snapshot first
/// (invariant #10: results are stable within a Turn).
#[derive(Debug, Clone)]
pub struct MemoryContextSnapshot {
    /// The Turn this snapshot was captured for.
    pub turn_id: TurnId,
    /// The index generation at capture time. If the live generation
    /// changes, the snapshot is stale and must be rebuilt.
    pub index_generation: u64,
    /// Cached retrieval results keyed by query fingerprint.
    /// When a query with the same fingerprint arrives, we return the
    /// cached result instead of re-querying the database.
    cache: HashMap<QueryFingerprint, CachedRetrieval>,
}

/// A cached retrieval result with its captured content.
#[derive(Debug, Clone)]
struct CachedRetrieval {
    /// The retrieval result frozen at capture time.
    retrieval: CombinedRetrieval,
    /// Wall-clock time the snapshot was taken (for diagnostics).
    captured_at: chrono::DateTime<chrono::Utc>,
}

impl MemoryContextSnapshot {
    /// Create a new empty snapshot for the given Turn.
    pub fn new(turn_id: TurnId, index_generation: u64) -> Self {
        Self {
            turn_id,
            index_generation,
            cache: HashMap::new(),
        }
    }

    /// Look up a cached retrieval by query fingerprint.
    ///
    /// Returns `Some(&CombinedRetrieval)` if the snapshot has a cached
    /// result for this exact fingerprint AND the index generation
    /// matches the current live generation (invariant #10).
    ///
    /// Returns `None` if:
    ///   - No cached entry exists for this fingerprint
    ///   - The live index generation has advanced past the snapshot's
    ///     (caller should rebuild the snapshot)
    pub fn get(
        &self,
        fingerprint: &QueryFingerprint,
        live_index_generation: u64,
    ) -> Option<&CombinedRetrieval> {
        // Invariant #10: if the index generation has changed, the
        // snapshot is stale — refuse to serve cached data.
        debug_assert!(
            live_index_generation >= self.index_generation,
            "invariant #10: live generation {} must not precede snapshot generation {}",
            live_index_generation,
            self.index_generation,
        );
        if live_index_generation != self.index_generation {
            return None; // index changed → snapshot stale
        }
        self.cache
            .get(fingerprint)
            .map(|cached| &cached.retrieval)
    }

    /// Insert a retrieval result into the cache for a given fingerprint.
    ///
    /// Called at Turn start after the initial retrieval completes, or
    /// when a new query is performed and the result should be cached
    /// for the remainder of the Turn.
    pub fn insert(
        &mut self,
        fingerprint: QueryFingerprint,
        retrieval: CombinedRetrieval,
    ) {
        self.cache.insert(
            fingerprint,
            CachedRetrieval {
                retrieval,
                captured_at: chrono::Utc::now(),
            },
        );
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// The Turn this snapshot belongs to.
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// The index generation at snapshot time.
    pub fn index_generation(&self) -> u64 {
        self.index_generation
    }
}

/// Thread-safe handle to the current Turn's memory snapshot.
///
/// The supervisor holds this and passes it to the turn coordinator.
/// When a new Turn starts, `replace()` installs a fresh snapshot;
/// within a Turn, `get()` returns the frozen view.
#[derive(Debug, Clone)]
pub struct MemorySnapshotHandle {
    inner: Arc<RwLock<Option<MemoryContextSnapshot>>>,
}

impl MemorySnapshotHandle {
    /// Create a new handle with no active snapshot.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
        }
    }

    /// Install a new snapshot (typically at Turn start).
    ///
    /// Invariant #10: after `replace()`, all subsequent `get()` calls
    /// within the Turn see the same frozen data regardless of external
    /// changes to the memory database.
    pub fn replace(&self, snapshot: MemoryContextSnapshot) {
        let mut guard = self.inner.write().expect("memory snapshot lock poisoned");
        *guard = Some(snapshot);
    }

    /// Clear the current snapshot (typically at Turn end).
    pub fn clear(&self) {
        let mut guard = self.inner.write().expect("memory snapshot lock poisoned");
        *guard = None;
    }

    /// Look up a cached retrieval from the current snapshot.
    ///
    /// Returns `None` if:
    ///   - No snapshot is active (between Turns)
    ///   - The fingerprint is not cached
    ///   - The index generation has advanced (snapshot stale)
    pub fn get(
        &self,
        fingerprint: &QueryFingerprint,
        live_index_generation: u64,
    ) -> Option<CombinedRetrieval> {
        let guard = self.inner.read().expect("memory snapshot lock poisoned");
        guard.as_ref().and_then(|snap| {
            snap.get(fingerprint, live_index_generation).cloned()
        })
    }

    /// Insert a retrieval into the current snapshot's cache.
    ///
    /// Returns `false` if no snapshot is active (caller should create one).
    pub fn insert(
        &self,
        fingerprint: QueryFingerprint,
        retrieval: CombinedRetrieval,
    ) -> bool {
        let mut guard = self.inner.write().expect("memory snapshot lock poisoned");
        if let Some(snap) = guard.as_mut() {
            snap.insert(fingerprint, retrieval);
            true
        } else {
            false
        }
    }

    /// Whether a snapshot is currently active.
    pub fn is_active(&self) -> bool {
        let guard = self.inner.read().expect("memory snapshot lock poisoned");
        guard.is_some()
    }

    /// The Turn id of the active snapshot, if any.
    pub fn active_turn_id(&self) -> Option<TurnId> {
        let guard = self.inner.read().expect("memory snapshot lock poisoned");
        guard.as_ref().map(|s| s.turn_id)
    }
}

impl Default for MemorySnapshotHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_memory::retrievers::CombinedRetrieval;

    fn make_fingerprint(query: &str) -> QueryFingerprint {
        QueryFingerprint::from_query(query)
    }

    fn make_empty_retrieval() -> CombinedRetrieval {
        CombinedRetrieval {
            skills: vec![],
            memory: vec![],
            evidence: vec![],
            total_memory_evidence: 0,
            diagnostics: vec![],
        }
    }

    #[test]
    fn snapshot_caches_and_retrieves_by_fingerprint() {
        let turn_id = TurnId::new();
        let mut snap = MemoryContextSnapshot::new(turn_id, 5);

        let fp = make_fingerprint("how does auth work");
        let retrieval = make_empty_retrieval();
        snap.insert(fp.clone(), retrieval.clone());

        // Same generation → cache hit
        let cached = snap.get(&fp, 5);
        assert!(cached.is_some(), "same generation should hit cache");

        // Different generation → cache miss (stale)
        let cached = snap.get(&fp, 6);
        assert!(cached.is_none(), "different generation should miss cache");

        // Unknown fingerprint → cache miss
        let fp2 = make_fingerprint("what is deployment");
        let cached = snap.get(&fp2, 5);
        assert!(cached.is_none(), "unknown fingerprint should miss");
    }

    #[test]
    fn snapshot_invariant_10_generation_must_not_regress() {
        let turn_id = TurnId::new();
        let snap = MemoryContextSnapshot::new(turn_id, 10);

        let fp = make_fingerprint("test query");

        // live > snapshot generation → snapshot is stale, returns None
        // (we test the forward direction: live=11 > snapshot=10)
        let cached = snap.get(&fp, 11);
        assert!(cached.is_none(), "advanced generation should miss cache");

        // live == snapshot → cache hit (if entry exists)
        let mut snap2 = MemoryContextSnapshot::new(TurnId::new(), 10);
        snap2.insert(fp.clone(), make_empty_retrieval());
        let cached = snap2.get(&fp, 10);
        assert!(cached.is_some(), "matching generation should hit cache");
    }

    #[test]
    fn handle_replace_and_get() {
        let handle = MemorySnapshotHandle::new();
        assert!(!handle.is_active());

        let turn_id = TurnId::new();
        let fp = make_fingerprint("test");
        let mut snap = MemoryContextSnapshot::new(turn_id, 1);
        snap.insert(fp.clone(), make_empty_retrieval());

        handle.replace(snap);
        assert!(handle.is_active());
        assert_eq!(handle.active_turn_id(), Some(turn_id));

        // Cache hit through handle
        let result = handle.get(&fp, 1);
        assert!(result.is_some());

        // Different generation → miss
        let result = handle.get(&fp, 2);
        assert!(result.is_none());
    }

    #[test]
    fn handle_clear_removes_snapshot() {
        let handle = MemorySnapshotHandle::new();
        let turn_id = TurnId::new();
        let snap = MemoryContextSnapshot::new(turn_id, 1);
        handle.replace(snap);
        assert!(handle.is_active());

        handle.clear();
        assert!(!handle.is_active());
        assert_eq!(handle.active_turn_id(), None);
    }

    #[test]
    fn handle_insert_without_active_snapshot_returns_false() {
        let handle = MemorySnapshotHandle::new();
        let fp = make_fingerprint("orphan query");
        assert!(!handle.insert(fp, make_empty_retrieval()));
    }

    #[test]
    fn handle_insert_with_active_snapshot_succeeds() {
        let handle = MemorySnapshotHandle::new();
        let turn_id = TurnId::new();
        let snap = MemoryContextSnapshot::new(turn_id, 1);
        handle.replace(snap);

        let fp = make_fingerprint("late query");
        assert!(handle.insert(fp.clone(), make_empty_retrieval()));

        // Now get should work
        let result = handle.get(&fp, 1);
        assert!(result.is_some());
    }
}
