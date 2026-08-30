//! ContextProjection — the lossy, replaceable model-visible view.
//!
//! Design Doc 11 §3: the projection is separate from the immutable rollout.
//! - rollout.jsonl: append-only, complete, for audit/recovery
//! - ContextProjection: lossy, replaceable, for model consumption
//!
//! Compaction replaces the projection without touching rollout facts.
//! Tool outputs can be truncated in the projection while rollout keeps full content.
//!
//! Design Doc 11 §6 defines 10 metadata fields on the projection for
//! bookkeeping, replay, and cache invalidation. These carry identifiers
//! rather than inline blobs so the projection stays small; the actual
//! blobs live in rollout or an external store.

use grodex_core::context::ContextItem;
use grodex_rollout::event::{RolloutEvent, RolloutEventType};
use serde::{Deserialize, Serialize};

/// A three-level layered view of context (Design Doc 11 §10).
///
/// Each level has a different volatility and cache lifetime:
///   - Level 0: Preserved (Zone A + pinned Developer items) — stable across compaction
///   - Level 1: Summary (CompactionSummaries + StateCapsule) — changes per-compaction
///   - Level 2: Recent (verbatim tail + ongoing Turn) — changes per Step
#[derive(Debug, Clone)]
pub struct LayeredContext<'a> {
    /// Level 0 — preserved verbatim across compactions (system instructions,
    /// pinned developer/project rules). Returned as references.
    pub level0_preserved: Vec<&'a ContextItem>,
    /// Level 1 — compaction summaries + state capsule. Replaced each compaction.
    pub level1_summary: Vec<&'a ContextItem>,
    /// Level 2 — recent verbatim items (last N turns, ongoing turn).
    pub level2_recent: Vec<&'a ContextItem>,
}

/// The model-visible context projection.
///
/// Built from rollout events via `from_rollout()`. Replaced atomically
/// by compaction. Maintains a `history_version` for cache invalidation
/// and `source_seq_end` for recovery fence checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextProjection {
    /// Items visible to the model (may be truncated/summarized).
    items: Vec<ContextItem>,
    /// Monotonic version, bumped on compaction or truncation.
    history_version: u64,
    /// Last rollout seq number included in this projection.
    source_seq_end: u64,

    // ── Doc 11 §6 metadata fields (added in this pass) ──────────────────
    /// Stable checkpoint id — the projection can be rebuilt from rollout
    /// starting at this checkpoint + incremental events after.
    checkpoint_id: Option<String>,
    /// Incremental token total maintained by `append` — avoids O(n)
    /// re-summation of `estimated_tokens()` per pushed item (O(n²)/turn).
    /// Recomputed wholesale by `replace`.
    total_est_tokens: u64,
    /// Accounting: actual_tokens, budget_tokens_remaining, soft_limit_exceeded.
    /// None when no model has been sampled against this projection yet.
    token_accounting: Option<TokenAccounting>,
    /// Version of the maintenance/cleanup policy (trim thresholds, quotas).
    /// Bumped when maintenance settings change so cached projections are invalidated.
    maintenance_policy_version: u64,
    /// Optional pointer to a reference context (e.g. another session's
    /// rollout range) — populated for cross-session "continue as" or
    /// sub-agent work. The ref is `(session_id, seq_start, seq_end)`.
    reference_context: Option<ContextReference>,
    /// World-state baseline hash (filesystem snapshot hash, rollout prefix
    /// hash) used for "did the env change since last turn" validation.
    world_state_baseline: Option<String>,
    /// Identifier for the latest state-capsule (from a prior compaction).
    /// Allows incremental state-capsule updates rather than full rebuilds.
    state_capsule_id: Option<String>,
}

/// Token accounting for a context projection (Design Doc 11 §6.4).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct TokenAccounting {
    /// Estimated tokens in the current assembled projection (items).
    pub actual_tokens: u64,
    /// Token budget remaining before the next mandatory compaction.
    pub budget_tokens_remaining: u64,
    /// True if the projection has exceeded a soft warning threshold (callers
    /// should schedule compaction on the next Turn boundary).
    pub soft_limit_exceeded: bool,
    /// Token budget configuration used to compute `budget_tokens_remaining`
    /// (for reproducibility during replay).
    pub budget_total_tokens: u64,
}

/// A reference to another context range (Design Doc 11 §6.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextReference {
    /// Session id the reference points at.
    pub session_id: String,
    /// Inclusive start seq in the referenced session's rollout.
    pub seq_start: u64,
    /// Exclusive end seq in the referenced session's rollout.
    pub seq_end: u64,
    /// Human-readable label (e.g. "parent-session-base", "context-share-foo").
    pub label: Option<String>,
}

impl ContextProjection {
    /// Create an empty projection.
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            history_version: 0,
            source_seq_end: 0,
            checkpoint_id: None,
            total_est_tokens: 0,
            token_accounting: None,
            maintenance_policy_version: 1,
            reference_context: None,
            world_state_baseline: None,
            state_capsule_id: None,
        }
    }

    // ── Builder-style setters for the Doc 11 §6 metadata ──────────────

    /// Attach a checkpoint id (rollout checkpoint from which this projection
    /// can be rebuilt by replaying events from `checkpoint_id..source_seq_end`).
    pub fn with_checkpoint_id(mut self, id: impl Into<String>) -> Self {
        self.checkpoint_id = Some(id.into());
        self
    }
    /// Stamp token accounting after sampling so the next Turn knows whether
    /// compaction is due (and the budget can be audited during replay).
    pub fn with_token_accounting(mut self, a: TokenAccounting) -> Self {
        self.token_accounting = Some(a);
        self
    }
    /// Bump (or explicitly set) the maintenance-policy version — used when
    /// trim thresholds or quotas change, to invalidate caches of projections
    /// produced under an older policy.
    pub fn with_maintenance_policy_version(mut self, v: u64) -> Self {
        self.maintenance_policy_version = v;
        self
    }
    /// Attach a reference to another session's rollout range (cross-session
    /// continue-as, sub-agent base context).
    pub fn with_reference(mut self, r: ContextReference) -> Self {
        self.reference_context = Some(r);
        self
    }
    /// Attach the world-state baseline hash (fs snapshot or rollout prefix)
    /// for drift detection before the next Turn.
    pub fn with_world_state_baseline(mut self, hash: impl Into<String>) -> Self {
        self.world_state_baseline = Some(hash.into());
        self
    }
    /// Record the id of the latest state-capsule produced by compaction so
    /// incremental compactions can build on top of it instead of from scratch.
    pub fn with_state_capsule_id(mut self, id: impl Into<String>) -> Self {
        self.state_capsule_id = Some(id.into());
        self
    }

    // ── Metadata accessors ────────────────────────────────────────────
    pub fn checkpoint_id(&self) -> Option<&str> { self.checkpoint_id.as_deref() }
    pub fn token_accounting(&self) -> Option<TokenAccounting> { self.token_accounting }
    pub fn maintenance_policy_version(&self) -> u64 { self.maintenance_policy_version }
    pub fn reference_context(&self) -> Option<&ContextReference> { self.reference_context.as_ref() }
    pub fn world_state_baseline(&self) -> Option<&str> { self.world_state_baseline.as_deref() }
    pub fn state_capsule_id(&self) -> Option<&str> { self.state_capsule_id.as_deref() }

    /// Build a projection from rollout events.
    /// System/Developer items are preserved. User/Assistant/Tool items
    /// are included verbatim. CompactionSummaries replace previous content.
    pub fn from_rollout(events: &[RolloutEvent]) -> Self {
        let mut items = Vec::new();
        let mut max_seq = 0u64;

        for event in events {
            max_seq = max_seq.max(event.seq);
            match event.event_type {
                RolloutEventType::UserInputAccepted => {
                    if let Some(text) = event.payload.get("text").and_then(|v| v.as_str()) {
                        items.push(ContextItem::User { content: text.to_string(), message_id: None });
                    }
                }
                RolloutEventType::ModelItemProduced => {
                    if let Some(text) = event.payload.get("assistant_text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            items.push(ContextItem::Assistant { content: text.to_string() });
                        }
                    }
                    if let Some(tc_list) = event.payload.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tc_list {
                            if let (Some(name), Some(args)) = (
                                tc.get("name").and_then(|v| v.as_str()),
                                tc.get("arguments"),
                            ) {
                                let call_id = tc.get("call_id")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| grodex_core::id::ToolCallId::from_string(s).ok())
                                    .unwrap_or_else(grodex_core::id::ToolCallId::new);
                                items.push(ContextItem::ToolCall {
                                    call_id, name: name.to_string(), arguments: args.clone(),
                                });
                            }
                        }
                    }
                }
                RolloutEventType::ToolResultCommitted => {
                    if let (Some(cid), Some(content)) = (
                        event.payload.get("call_id").and_then(|v| v.as_str()),
                        event.payload.get("content").and_then(|v| v.as_str()),
                    ) {
                        let call_id = grodex_core::id::ToolCallId::from_string(cid).unwrap_or_default();
                        let is_error = event.payload.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                        items.push(ContextItem::ToolResult { call_id, content: content.to_string(), is_error });
                    }
                }
                RolloutEventType::CompactionCommitted => {
                    if let Some(arr) = event.payload.get("items").and_then(|v| v.as_array()) {
                        let rebuilt: Vec<ContextItem> = arr.iter()
                            .filter_map(|v| serde_json::from_value(v.clone()).ok())
                            .collect();
                        if !rebuilt.is_empty() { items = rebuilt; }
                    }
                }
                _ => {}
            }
        }

        let total_est_tokens = items.iter().map(|i| i.estimated_tokens() as u64).sum();
        let mut p = Self { items, total_est_tokens, history_version: 1, source_seq_end: max_seq,
            checkpoint_id: None, token_accounting: None,
            maintenance_policy_version: 1, reference_context: None,
            world_state_baseline: None, state_capsule_id: None,
        };
        // Stamp a rough token accounting estimate so early callers don't see None.
        let est = p.estimated_tokens();
        let budget: u64 = 128_000; // default; real callers override via with_token_accounting()
        p.token_accounting = Some(TokenAccounting {
            actual_tokens: est,
            budget_tokens_remaining: budget.saturating_sub(est),
            soft_limit_exceeded: est > budget * 80 / 100,
            budget_total_tokens: budget,
        });
        p
    }

    /// Atomically replace the projection (compaction).
    pub fn replace(&mut self, new_items: Vec<ContextItem>) {
        self.total_est_tokens =
            new_items.iter().map(|i| i.estimated_tokens() as u64).sum();
        self.items = new_items;
        self.history_version += 1;
        // Recompute token accounting after compaction (best-effort).
        let est = self.estimated_tokens();
        let budget = self.token_accounting.map(|a| a.budget_total_tokens).unwrap_or(128_000);
        self.token_accounting = Some(TokenAccounting {
            actual_tokens: est,
            budget_tokens_remaining: budget.saturating_sub(est),
            soft_limit_exceeded: est > budget * 80 / 100,
            budget_total_tokens: budget,
        });
    }

    /// Append an item from the live transcript (during a Turn).
    pub fn append(&mut self, item: ContextItem, seq: u64) {
        // Incremental token accounting: the previous implementation
        // re-summed estimated_tokens() over the WHOLE projection on every
        // push — O(n) per item, O(n²) per turn, with a JSON re-serialize
        // per ToolCall item each time. Track the delta instead.
        let item_est = item.estimated_tokens() as u64;
        self.items.push(item);
        self.source_seq_end = seq;
        self.history_version += 1;
        self.total_est_tokens = self.total_est_tokens.saturating_add(item_est);
        if let Some(ref mut a) = self.token_accounting {
            let est = self.total_est_tokens;
            a.actual_tokens = est;
            a.budget_tokens_remaining = a.budget_total_tokens.saturating_sub(est);
            a.soft_limit_exceeded = est > a.budget_total_tokens * 80 / 100;
        }
    }

    /// Get the items for model consumption (legacy — full flat list).
    pub fn for_model(&self) -> &[ContextItem] {
        &self.items
    }

    /// Layered view (Design Doc 11 §10). Splits the items into three
    /// cache-friendly levels: Preserved (Zone A / Developer pinned),
    /// Summary (Compaction summaries / state capsule), Recent (tail).
    ///
    /// Heuristic classification — callers can pass in a predicate to
    /// override which items count as "preserved" (e.g. Zone A system
    /// instructions the runtime knows about). When `is_preserved` is
    /// None we treat System and Developer items as preserved.
    pub fn for_model_layered<'a, F>(&'a self, is_preserved: Option<F>) -> LayeredContext<'a>
    where
        F: Fn(&ContextItem) -> bool,
    {
        let mut level0_preserved = Vec::new();
        let mut level1_summary = Vec::new();
        let mut level2_recent = Vec::new();

        // CompactionSummary / StateCapsule marker items sit between the
        // preserved block and the recent tail. We approximate by scanning:
        //   - everything before the first non-preserved non-marker item
        //     + any CompactionSummary item → level 1
        //   - everything else → level 2
        // (Exact boundaries are owned by the Compactor; this layered view
        // is purely an advisory structure for cache invalidation.)
        let preserved_pred = |item: &ContextItem| -> bool {
            match &is_preserved {
                Some(pred) => pred(item),
                None => matches!(item, ContextItem::System { .. } | ContextItem::Developer { .. }),
            }
        };

        // Simple heuristic split: preserved first, then the last 8 items as
        // "recent tail", middle as "summary". The Compactor produces a much
        // sharper split, but this is reasonable for runtime caching purposes.
        let tail_start = if self.items.len() > 8 { self.items.len() - 8 } else { 0 };

        for (idx, item) in self.items.iter().enumerate() {
            if preserved_pred(item) {
                level0_preserved.push(item);
            } else if idx < tail_start {
                level1_summary.push(item);
            } else {
                level2_recent.push(item);
            }
        }

        LayeredContext { level0_preserved, level1_summary, level2_recent }
    }

    /// Get the items as owned Vec.
    pub fn into_items(self) -> Vec<ContextItem> {
        self.items
    }

    /// Number of items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the projection is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Current history version.
    pub fn history_version(&self) -> u64 {
        self.history_version
    }

    /// Last source seq covered.
    pub fn source_seq_end(&self) -> u64 {
        self.source_seq_end
    }

    /// Estimate total tokens (incremental — maintained by `append`/`replace`).
    pub fn estimated_tokens(&self) -> u64 {
        self.total_est_tokens
    }
}

impl Default for ContextProjection {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use grodex_core::id::SessionId;
    use grodex_rollout::event::{RolloutEvent, RolloutEventType, SensitivityLevel};

    fn make_event(seq: u64, event_type: RolloutEventType, payload: serde_json::Value) -> RolloutEvent {
        RolloutEvent {
            schema_version: 2, seq, session_id: SessionId::new(),
            turn_id: None, step_id: None, generation: None,
            timestamp: Utc::now(), event_type, payload,
            sensitivity: SensitivityLevel::Normal,
        }
    }

    #[test]
    fn builds_from_rollout_events() {
        let events = vec![
            make_event(0, RolloutEventType::UserInputAccepted, serde_json::json!({"text": "hello"})),
            make_event(1, RolloutEventType::ModelItemProduced, serde_json::json!({"assistant_text": "hi there"})),
        ];
        let proj = ContextProjection::from_rollout(&events);
        assert_eq!(proj.len(), 2);
        assert_eq!(proj.source_seq_end(), 1);
        assert!(matches!(proj.for_model()[0], ContextItem::User { .. }));
        // Token accounting should have been stamped automatically.
        let acc = proj.token_accounting().expect("default accounting");
        assert!(acc.actual_tokens > 0);
        assert_eq!(acc.budget_total_tokens, 128_000);
    }

    #[test]
    fn replace_bumps_version_and_refreshes_accounting() {
        let mut proj = ContextProjection::new();
        proj.append(ContextItem::User { content: "old".into(), message_id: None }, 0);
        let v1 = proj.history_version();
        let acc_before = proj.token_accounting();
        proj.replace(vec![ContextItem::System { content: "new".into() }]);
        assert!(proj.history_version() > v1);
        assert_eq!(proj.len(), 1);
        // Accounting after replace should show the new content (shorter).
        let acc_after = proj.token_accounting().unwrap();
        assert!(acc_after.actual_tokens > 0);
        match (acc_before, acc_after) {
            (Some(b), a) => assert_ne!(b.actual_tokens, a.actual_tokens),
            _ => {}
        }
    }

    #[test]
    fn metadata_setters_and_accessors_roundtrip() {
        let proj = ContextProjection::new()
            .with_checkpoint_id("cp_123")
            .with_maintenance_policy_version(42)
            .with_world_state_baseline("hash_abc")
            .with_state_capsule_id("sc_789")
            .with_token_accounting(TokenAccounting {
                actual_tokens: 50_000,
                budget_tokens_remaining: 78_000,
                soft_limit_exceeded: false,
                budget_total_tokens: 128_000,
            })
            .with_reference(ContextReference {
                session_id: "sid_parent".into(),
                seq_start: 0,
                seq_end: 42,
                label: Some("parent".into()),
            });

        assert_eq!(proj.checkpoint_id(), Some("cp_123"));
        assert_eq!(proj.maintenance_policy_version(), 42);
        assert_eq!(proj.world_state_baseline(), Some("hash_abc"));
        assert_eq!(proj.state_capsule_id(), Some("sc_789"));
        let acc = proj.token_accounting().unwrap();
        assert_eq!(acc.actual_tokens, 50_000);
        assert_eq!(acc.budget_total_tokens, 128_000);
        let r = proj.reference_context().unwrap();
        assert_eq!(r.session_id, "sid_parent");
        assert_eq!(r.seq_end, 42);
    }

    #[test]
    fn for_model_layered_splits_preserved_summary_recent() {
        let mut proj = ContextProjection::new();
        proj.append(ContextItem::System { content: "Zone A system".into() }, 0);
        proj.append(ContextItem::Developer { content: "pinned rule".into() }, 1);
        // A bunch of older user/assistant turns (middle block = summary).
        for i in 0..15 {
            proj.append(ContextItem::User { content: format!("old user {i}"), message_id: None }, 2 + i * 2);
            proj.append(ContextItem::Assistant { content: format!("old reply {i}") }, 3 + i * 2);
        }
        proj.append(ContextItem::User { content: "current query".into(), message_id: None }, 100);

        let layered = proj.for_model_layered::<fn(&ContextItem) -> bool>(None);
        // Level 0 should have the system+developer items.
        assert!(layered.level0_preserved.iter().any(|i| matches!(i, ContextItem::System { .. })));
        assert!(layered.level0_preserved.iter().any(|i| matches!(i, ContextItem::Developer { .. })));
        // Last item should be in the recent (level2) block.
        let last_recent = layered.level2_recent.last().unwrap();
        assert!(matches!(last_recent, ContextItem::User { content, .. } if content == "current query"));
        // Summary (level1) should contain the bulk of older conversation.
        assert!(!layered.level1_summary.is_empty());
    }
}
