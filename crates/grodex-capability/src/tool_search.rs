//! Deferred promotion — the complete Tool Search flow (Doc 10 §17.3).
//!
//! The flow this module implements end-to-end:
//!
//! 1. `DeferredToolIndex::build` captures ONLY the Deferred descriptors of
//!    the `TurnCapabilityBase`; Hidden / Internal / AppOnly / Disabled and
//!    Policy-excluded capabilities never enter the search index.
//! 2. `DeferredToolIndex::search` matches the query against the index and
//!    returns hits pinned to the revision observed AT SEARCH TIME, together
//!    with a deterministic query hash. If the Turn forbids deferred
//!    promotion the search fails closed with an explicit reason.
//! 3. `PromotionPlanner::record_hits` writes
//!    `promoted_capabilities[CapabilityId] = capability_revision` for the
//!    NEXT Step (never mutating the published state).
//! 4. `PromotionPlanner::plan_overlay` re-checks the pinned revision when
//!    the next Step is assembled: a changed revision is recorded as stale
//!    (never substituted with the new definition) and the capability must
//!    be re-searched; unchanged hits become `deferred_promoted_ids` plus
//!    one [`CapabilityPromotedEvent`] each, so replay can explain why a
//!    Step shows one more Direct tool than the Turn baseline.
//!
//! Acceptance #6: Deferred tools cost no initial schema budget; a search
//! hit writes a revision-pinned Turn overlay + `CapabilityPromoted`, and
//! the next Step can call it.

use crate::descriptor::TurnCapabilityBase;
use crate::exposure::ToolExposure;
use crate::id::CapabilityId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// One search index entry: a Deferred descriptor captured at Turn start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredIndexEntry {
    pub capability_id: CapabilityId,
    /// Revision of the capability when the index was built.
    pub revision: u64,
    /// Exposure carried for diagnostics — always `Deferred` in a valid
    /// index; kept so misuse is visible rather than implicit.
    pub exposure: ToolExposure,
    /// Human-readable display name matched against queries.
    pub display_name: String,
    /// One-line description matched against queries.
    pub description: String,
}

/// Why a candidate capability was refused entry into the search index
/// (Doc 10 §17.3: Tool Search must not return Hidden, Internal, AppOnly
/// or Policy-excluded capabilities).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexExclusionReason {
    /// Exposure is not `Deferred` (Direct is already in the model request;
    /// Hidden/Internal/AppOnly/Disabled/CodeMode are never searchable).
    NotDeferred,
    /// The id was not part of the Turn's promotable closure.
    NotInTurnBase,
    /// Excluded by the active Policy projection.
    PolicyExcluded,
}

/// Diagnostic record for every capability that was offered to the index
/// builder but not admitted — makes "why can't I search X" explainable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexExclusion {
    pub capability_id: CapabilityId,
    pub reason: IndexExclusionReason,
}

/// The per-Turn search index over Deferred capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeferredToolIndex {
    pub turn_id: String,
    pub entries: Vec<DeferredIndexEntry>,
    pub exclusions: Vec<IndexExclusion>,
}

/// One search hit: the descriptor plus the revision pinned at hit time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub capability_id: CapabilityId,
    /// Revision pinned at search-hit time (Doc 10 §17.3).
    pub pinned_revision: u64,
    pub display_name: String,
    pub description: String,
    /// Deterministic hash of the normalized query — carried into every
    /// `CapabilityPromotedEvent` produced from this search.
    pub query_hash: String,
}

/// Outcome of a Tool Search invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchOutcome {
    /// The Turn forbids deferred promotion — nothing is searchable
    /// (fail-closed, acceptance #6 enterprise path).
    PromotionNotAllowed,
    /// Searchable; may be empty (no match).
    Hits(Vec<SearchHit>),
}

impl DeferredToolIndex {
    /// Build the search index for one Turn.
    ///
    /// `candidates` is the full capability set considered at Turn start
    /// (id, exposure, revision, display name, description);
    /// `policy_excluded` lists ids the active Policy projection removes.
    /// Only entries with exposure `Deferred` AND present in
    /// `base.promotable_ids` AND not policy-excluded are admitted.
    pub fn build(
        base: &TurnCapabilityBase,
        candidates: &[(CapabilityId, ToolExposure, u64, String, String)],
        policy_excluded: &[CapabilityId],
    ) -> Self {
        let mut entries = Vec::new();
        let mut exclusions = Vec::new();
        for (id, exposure, revision, display_name, description) in candidates {
            if *exposure != ToolExposure::Deferred {
                exclusions.push(IndexExclusion {
                    capability_id: id.clone(),
                    reason: IndexExclusionReason::NotDeferred,
                });
                continue;
            }
            if !base.is_promotable(id) {
                exclusions.push(IndexExclusion {
                    capability_id: id.clone(),
                    reason: IndexExclusionReason::NotInTurnBase,
                });
                continue;
            }
            if policy_excluded.contains(id) {
                exclusions.push(IndexExclusion {
                    capability_id: id.clone(),
                    reason: IndexExclusionReason::PolicyExcluded,
                });
                continue;
            }
            entries.push(DeferredIndexEntry {
                capability_id: id.clone(),
                revision: *revision,
                exposure: *exposure,
                display_name: display_name.clone(),
                description: description.clone(),
            });
        }
        Self { turn_id: base.turn_id.clone(), entries, exclusions }
    }

    /// Deterministic hash over the normalized query (lowercased, trimmed,
    /// interior whitespace collapsed) — stable across replays.
    pub fn query_hash(query: &str) -> String {
        let normalized: String = query
            .trim()
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut h = Sha256::new();
        h.update(normalized.as_bytes());
        format!("{:x}", h.finalize())[..16].to_string()
    }

    /// Run a Tool Search. Fails closed when the Turn forbids deferred
    /// promotion; otherwise returns all entries whose display name or
    /// description contain every whitespace-separated query term
    /// (case-insensitive).
    pub fn search(&self, base: &TurnCapabilityBase, query: &str) -> SearchOutcome {
        if !base.deferred_promotion_allowed {
            return SearchOutcome::PromotionNotAllowed;
        }
        let qh = Self::query_hash(query);
        let terms: Vec<String> =
            query.trim().to_lowercase().split_whitespace().map(str::to_string).collect();
        let mut hits: Vec<SearchHit> = self
            .entries
            .iter()
            .filter(|e| {
                let name = e.display_name.to_lowercase();
                let desc = e.description.to_lowercase();
                terms.iter().all(|t| name.contains(t.as_str()) || desc.contains(t.as_str()))
            })
            .map(|e| SearchHit {
                capability_id: e.capability_id.clone(),
                pinned_revision: e.revision,
                display_name: e.display_name.clone(),
                description: e.description.clone(),
                query_hash: qh.clone(),
            })
            .collect();
        hits.sort_by(|a, b| a.capability_id.cmp(&b.capability_id));
        SearchOutcome::Hits(hits)
    }
}

/// Durable event explaining why a Step shows one more Direct tool than the
/// Turn baseline (Doc 10 §17.3, persisted into rollout.jsonl — §24).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityPromotedEvent {
    pub turn_id: String,
    /// Hash of the search query that produced this promotion.
    pub query_hash: String,
    pub capability_id: CapabilityId,
    /// Revision pinned at search-hit time.
    pub pinned_revision: u64,
    /// Step where the Tool Search ran.
    pub source_step_index: usize,
    /// Step whose Router gains the Direct exposure.
    pub target_step_index: usize,
}

/// A promotion whose pinned revision no longer matches the published state
/// when the target Step is assembled (Doc 10 §17.3: record stale, never
/// impersonate the original hit with the new version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalePromotion {
    pub capability_id: CapabilityId,
    pub pinned_revision: u64,
    /// Revision actually published at assembly time.
    pub current_revision: u64,
}

/// Plans the Turn overlay from search hits, pinning revisions.
///
/// One planner per Turn; hits recorded at the source Step are materialized
/// when the NEXT Step's Router is assembled.
#[derive(Debug, Clone, Default)]
pub struct PromotionPlanner {
    turn_id: String,
    /// `promoted_capabilities[CapabilityId] = capability_revision` — the
    /// exact overlay map Doc 10 §17.3 prescribes for TurnContext.
    promoted_capabilities: BTreeMap<CapabilityId, (u64, String, usize)>,
}

/// Result of assembling the next Step's promotion set.
#[derive(Debug, Clone, Default)]
pub struct PlannedPromotions {
    /// Ids still matching their pinned revision — safe to add to the
    /// overlay's `deferred_promoted_ids`.
    pub valid_ids: Vec<CapabilityId>,
    /// Ids whose revision changed — excluded from the overlay; the caller
    /// must enter controlled resampling or require a re-search.
    pub stale: Vec<StalePromotion>,
    /// One event per valid promotion (audit trail).
    pub events: Vec<CapabilityPromotedEvent>,
}

impl PromotionPlanner {
    pub fn new(turn_id: impl Into<String>) -> Self {
        Self { turn_id: turn_id.into(), promoted_capabilities: BTreeMap::new() }
    }

    /// Record search hits produced at `source_step_index`. Later hits for
    /// the same capability id overwrite earlier ones (latest search wins).
    pub fn record_hits(&mut self, hits: &[SearchHit], source_step_index: usize) {
        for hit in hits {
            self.promoted_capabilities.insert(
                hit.capability_id.clone(),
                (hit.pinned_revision, hit.query_hash.clone(), source_step_index),
            );
        }
    }

    /// Whether the given capability is pending promotion for the next Step.
    pub fn pending(&self, id: &CapabilityId) -> bool {
        self.promoted_capabilities.contains_key(id)
    }

    /// Assemble the promotion set for `target_step_index`, re-checking each
    /// pinned revision against `current_revisions` (the published state at
    /// assembly time). Consumes the pending entries: valid promotions are
    /// realized, stale ones are reported and dropped (re-search required).
    pub fn plan_overlay(
        &mut self,
        target_step_index: usize,
        current_revisions: &BTreeMap<CapabilityId, u64>,
    ) -> PlannedPromotions {
        let mut out = PlannedPromotions::default();
        let pending = std::mem::take(&mut self.promoted_capabilities);
        for (id, (pinned_revision, query_hash, source_step_index)) in pending {
            match current_revisions.get(&id) {
                Some(&current) if current == pinned_revision => {
                    out.valid_ids.push(id.clone());
                    out.events.push(CapabilityPromotedEvent {
                        turn_id: self.turn_id.clone(),
                        query_hash,
                        capability_id: id,
                        pinned_revision,
                        source_step_index,
                        target_step_index,
                    });
                }
                Some(&current) => {
                    out.stale.push(StalePromotion {
                        capability_id: id,
                        pinned_revision,
                        current_revision: current,
                    });
                }
                // Capability vanished from the published state entirely —
                // also stale (cannot call something that no longer exists).
                None => {
                    out.stale.push(StalePromotion {
                        capability_id: id,
                        pinned_revision,
                        current_revision: 0,
                    });
                }
            }
        }
        out.valid_ids.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::Authority;
    use crate::descriptor::StepCapabilitySnapshot;
    use crate::id::CapabilityKind;

    fn id(name: &str) -> CapabilityId {
        CapabilityId::new(Authority::Core, "builtin", CapabilityKind::Tool, name)
    }

    fn base(promotable: Vec<CapabilityId>, deferred_allowed: bool) -> TurnCapabilityBase {
        let snaps: Vec<StepCapabilitySnapshot> = Vec::new();
        TurnCapabilityBase::new("turn-1", 7, 3, promotable, snaps, deferred_allowed, 0)
    }

    fn candidates() -> Vec<(CapabilityId, ToolExposure, u64, String, String)> {
        vec![
            (id("mcp.deploy"), ToolExposure::Deferred, 4, "deploy".into(), "Deploy the service".into()),
            (id("core.read"), ToolExposure::Direct, 1, "read".into(), "Read a file".into()),
            (id("internal.diag"), ToolExposure::Internal, 2, "diag".into(), "Internal diagnostics".into()),
            (id("app.focus"), ToolExposure::AppOnly, 1, "focus".into(), "UI focus action".into()),
        ]
    }

    #[test]
    fn index_admits_only_deferred_in_turn_base() {
        let b = base(vec![id("mcp.deploy"), id("core.read"), id("internal.diag"), id("app.focus")], true);
        let idx = DeferredToolIndex::build(&b, &candidates(), &[]);
        assert_eq!(idx.entries.len(), 1);
        assert_eq!(idx.entries[0].capability_id, id("mcp.deploy"));
        // Direct / Internal / AppOnly are each excluded with a reason.
        let reasons: Vec<IndexExclusionReason> =
            idx.exclusions.iter().map(|e| e.reason).collect();
        assert_eq!(reasons, vec![
            IndexExclusionReason::NotDeferred,
            IndexExclusionReason::NotDeferred,
            IndexExclusionReason::NotDeferred,
        ]);
    }

    #[test]
    fn policy_excluded_and_outside_turn_base_never_searchable() {
        // mcp.deploy in base but policy-excluded; mcp.new not in base at all.
        let b = base(vec![id("mcp.deploy")], true);
        let mut cands = candidates();
        cands.push((id("mcp.new"), ToolExposure::Deferred, 1, "new".into(), "Brand new".into()));
        let idx = DeferredToolIndex::build(&b, &cands, &[id("mcp.deploy")]);
        assert!(idx.entries.is_empty());
        assert!(idx.exclusions.iter().any(|e| e.reason == IndexExclusionReason::PolicyExcluded));
        assert!(idx.exclusions.iter().any(|e| e.reason == IndexExclusionReason::NotInTurnBase));
    }

    #[test]
    fn search_fails_closed_when_promotion_not_allowed() {
        let b = base(vec![id("mcp.deploy")], false);
        let idx = DeferredToolIndex::build(&b, &candidates(), &[]);
        assert_eq!(idx.search(&b, "deploy"), SearchOutcome::PromotionNotAllowed);
    }

    #[test]
    fn search_pins_revision_and_hash_is_deterministic() {
        let b = base(vec![id("mcp.deploy")], true);
        let idx = DeferredToolIndex::build(&b, &candidates(), &[]);
        let hits = match idx.search(&b, "  Deploy  ") {
            SearchOutcome::Hits(h) => h,
            other => panic!("expected hits, got {other:?}"),
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pinned_revision, 4);
        // Whitespace/case normalization: same hash as the clean form.
        assert_eq!(hits[0].query_hash, DeferredToolIndex::query_hash("deploy"));
        // No match returns an empty hit list, not an error.
        assert_eq!(idx.search(&b, "nonexistent"), SearchOutcome::Hits(vec![]));
    }

    #[test]
    fn planner_pins_revision_and_emits_promoted_event() {
        let b = base(vec![id("mcp.deploy")], true);
        let idx = DeferredToolIndex::build(&b, &candidates(), &[]);
        let hits = match idx.search(&b, "deploy") {
            SearchOutcome::Hits(h) => h,
            _ => panic!(),
        };
        let mut planner = PromotionPlanner::new("turn-1");
        planner.record_hits(&hits, 2);
        assert!(planner.pending(&id("mcp.deploy")));

        let mut current = BTreeMap::new();
        current.insert(id("mcp.deploy"), 4u64); // unchanged
        let planned = planner.plan_overlay(3, &current);
        assert_eq!(planned.valid_ids, vec![id("mcp.deploy")]);
        assert!(planned.stale.is_empty());
        let ev = &planned.events[0];
        assert_eq!(ev.pinned_revision, 4);
        assert_eq!(ev.source_step_index, 2);
        assert_eq!(ev.target_step_index, 3);
        assert_eq!(ev.query_hash, DeferredToolIndex::query_hash("deploy"));
        // Consumed: nothing pending after planning.
        assert!(!planner.pending(&id("mcp.deploy")));
    }

    #[test]
    fn changed_revision_is_stale_and_never_impersonated() {
        let b = base(vec![id("mcp.deploy")], true);
        let idx = DeferredToolIndex::build(&b, &candidates(), &[]);
        let hits = match idx.search(&b, "deploy") {
            SearchOutcome::Hits(h) => h,
            _ => panic!(),
        };
        let mut planner = PromotionPlanner::new("turn-1");
        planner.record_hits(&hits, 0);

        // Published state moved to revision 5 between search and assembly.
        let mut current = BTreeMap::new();
        current.insert(id("mcp.deploy"), 5u64);
        let planned = planner.plan_overlay(1, &current);
        assert!(planned.valid_ids.is_empty());
        assert!(planned.events.is_empty());
        assert_eq!(planned.stale, vec![StalePromotion {
            capability_id: id("mcp.deploy"),
            pinned_revision: 4,
            current_revision: 5,
        }]);

        // A vanished capability is stale too.
        planner.record_hits(&hits, 0);
        let planned = planner.plan_overlay(1, &BTreeMap::new());
        assert_eq!(planned.stale.len(), 1);
        assert_eq!(planned.stale[0].current_revision, 0);
    }
}
