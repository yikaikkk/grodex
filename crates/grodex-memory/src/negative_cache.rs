//! Session-level negative cache for retrieval.
//!
//! Design 08 §11: In a long session, the agent may repeat the same query
//! and repeatedly get empty results. The negative cache records:
//!   retriever_kind + normalized_query + index_generation → result_fingerprint
//!
//! Rules:
//! + Same retriever, same normalized query, same index generation → reuse empty result.
//! + Same result fingerprint → don't re-inject.
//! + Index generation change → invalidate.
//! + User explicit re-query → bypass cache.
//! + Not persisted across sessions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::types::ResultSource;

/// A cached negative (empty) or repeated result entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Which retriever produced this result.
    pub retriever_kind: ResultSource,
    /// Normalized query (whitespace-folded, trimmed only — no stemming).
    pub normalized_query: String,
    /// Index generation when the query was executed.
    pub index_generation: u64,
    /// Fingerprint of the result set (hash of unit IDs; empty = 0 results).
    pub result_fingerprint: String,
    /// Number of results returned.
    pub result_count: usize,
    /// Whether the result was consumed (injected into context).
    pub consumed: bool,
    /// When this cache entry was created.
    pub created_at: DateTime<Utc>,
}

/// Session-level negative cache. Not thread-safe by design — one per session.
///
/// Design 08 §11: prevents repeated empty-result queries within the same
/// session. Normalization is minimal (whitespace fold + trim) to avoid
/// different queries incorrectly sharing a cache entry.
#[derive(Debug, Default)]
pub struct NegativeCache {
    entries: HashMap<CacheKey, CacheEntry>,
}

/// Cache lookup key: retriever + normalized query + index generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    retriever_kind: ResultSource,
    normalized_query: String,
    index_generation: u64,
}

impl NegativeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalize a query: collapse consecutive whitespace, trim.
    /// No stopword removal, stemming, synonym expansion, or reordering.
    fn normalize(query: &str) -> String {
        query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    /// Check if a cached result exists for this query.
    ///
    /// Returns the cached entry if found and the index generation matches.
    /// Returns None if not cached or generation has changed.
    pub fn lookup(
        &self,
        retriever_kind: ResultSource,
        query: &str,
        index_generation: u64,
    ) -> Option<&CacheEntry> {
        let key = CacheKey {
            retriever_kind,
            normalized_query: Self::normalize(query),
            index_generation,
        };
        self.entries.get(&key)
    }

    /// Record a retrieval result in the cache.
    pub fn record(
        &mut self,
        retriever_kind: ResultSource,
        query: &str,
        index_generation: u64,
        result_ids: &[String],
        consumed: bool,
    ) {
        let key = CacheKey {
            retriever_kind,
            normalized_query: Self::normalize(query),
            index_generation,
        };
        let fingerprint = if result_ids.is_empty() {
            "empty".to_string()
        } else {
            // Simple hash: join sorted IDs.
            let mut sorted = result_ids.to_vec();
            sorted.sort();
            sorted.join(",")
        };
        self.entries.insert(
            key,
            CacheEntry {
                retriever_kind,
                normalized_query: Self::normalize(query),
                index_generation,
                result_fingerprint: fingerprint,
                result_count: result_ids.len(),
                consumed,
                created_at: Utc::now(),
            },
        );
    }

    /// Check if a query is cached as empty (negative cache hit).
    ///
    /// Returns true only if the cached result_count is 0 and the
    /// index generation matches.
    pub fn is_cached_empty(
        &self,
        retriever_kind: ResultSource,
        query: &str,
        index_generation: u64,
    ) -> bool {
        self.lookup(retriever_kind, query, index_generation)
            .map(|e| e.result_count == 0)
            .unwrap_or(false)
    }

    /// Check if a query has a cached result with the same fingerprint
    /// (to avoid re-injecting identical results).
    pub fn has_same_fingerprint(
        &self,
        retriever_kind: ResultSource,
        query: &str,
        index_generation: u64,
        result_ids: &[String],
    ) -> bool {
        let mut sorted = result_ids.to_vec();
        sorted.sort();
        let fingerprint = if sorted.is_empty() {
            "empty".to_string()
        } else {
            sorted.join(",")
        };
        self.lookup(retriever_kind, query, index_generation)
            .map(|e| e.result_fingerprint == fingerprint)
            .unwrap_or(false)
    }

    /// Mark a cached result as consumed (injected into context).
    pub fn mark_consumed(
        &mut self,
        retriever_kind: ResultSource,
        query: &str,
        index_generation: u64,
    ) {
        let key = CacheKey {
            retriever_kind,
            normalized_query: Self::normalize(query),
            index_generation,
        };
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.consumed = true;
        }
    }

    /// Invalidate all entries for a given index generation
    /// (called when generation changes).
    pub fn invalidate_generation(&mut self, old_generation: u64) {
        self.entries
            .retain(|_, v| v.index_generation != old_generation);
    }

    /// Clear the entire cache.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_result_is_cached() {
        let mut cache = NegativeCache::new();
        cache.record(
            ResultSource::Memory,
            "nonexistent query",
            1,
            &[],
            false,
        );
        assert!(cache.is_cached_empty(ResultSource::Memory, "nonexistent query", 1));
    }

    #[test]
    fn non_empty_result_is_not_negative_cached() {
        let mut cache = NegativeCache::new();
        cache.record(
            ResultSource::Memory,
            "rust release",
            1,
            &["mem_1".to_string(), "mem_2".to_string()],
            false,
        );
        assert!(!cache.is_cached_empty(ResultSource::Memory, "rust release", 1));
    }

    #[test]
    fn generation_change_invalidates() {
        let mut cache = NegativeCache::new();
        cache.record(ResultSource::Memory, "query", 1, &[], false);
        assert!(cache.is_cached_empty(ResultSource::Memory, "query", 1));
        // Generation bumped to 2 — old cache should not hit.
        assert!(!cache.is_cached_empty(ResultSource::Memory, "query", 2));
    }

    #[test]
    fn same_fingerprint_detected() {
        let mut cache = NegativeCache::new();
        let ids = vec!["mem_a".to_string(), "mem_b".to_string()];
        cache.record(ResultSource::Memory, "query", 1, &ids, false);
        // Same IDs in different order should match fingerprint.
        let reversed = vec!["mem_b".to_string(), "mem_a".to_string()];
        assert!(cache.has_same_fingerprint(ResultSource::Memory, "query", 1, &reversed));
    }

    #[test]
    fn different_retriever_does_not_share_cache() {
        let mut cache = NegativeCache::new();
        cache.record(ResultSource::Memory, "query", 1, &[], false);
        assert!(!cache.is_cached_empty(ResultSource::Evidence, "query", 1));
    }

    #[test]
    fn normalization_collapses_whitespace() {
        let mut cache = NegativeCache::new();
        cache.record(ResultSource::Memory, "rust   release", 1, &[], false);
        // Different whitespace should match.
        assert!(cache.is_cached_empty(ResultSource::Memory, "rust release", 1));
        assert!(cache.is_cached_empty(ResultSource::Memory, "  rust    release  ", 1));
    }

    #[test]
    fn mark_consumed_updates_entry() {
        let mut cache = NegativeCache::new();
        cache.record(ResultSource::Memory, "query", 1, &["mem_1".to_string()], false);
        assert!(!cache.lookup(ResultSource::Memory, "query", 1).unwrap().consumed);
        cache.mark_consumed(ResultSource::Memory, "query", 1);
        assert!(cache.lookup(ResultSource::Memory, "query", 1).unwrap().consumed);
    }

    #[test]
    fn clear_empties_cache() {
        let mut cache = NegativeCache::new();
        cache.record(ResultSource::Memory, "q1", 1, &[], false);
        cache.record(ResultSource::Evidence, "q2", 1, &[], false);
        assert_eq!(cache.len(), 2);
        cache.clear();
        assert!(cache.is_empty());
    }
}
