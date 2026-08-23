//! Context compaction — token-aware auto-compaction for long conversations.
//!
//! Following Grok's `xai-grok-compaction` architecture:
//!   1. Token budget detection (85% threshold)
//!   2. Item selection (walk backward, snap forward past tool runs)
//!   3. Compaction prompt → model → extract summary
//!   4. Assembly: [Zone A, summary, state capsule, recent]
//!   5. Validation (no orphaned ToolResults)
//!
//! Phase 1: full-replace strategy. Later phases will add intra-turn
//! (tail-keep) and two-pass prefire.

pub mod assembly;
pub mod compactor;
pub mod journal;
pub mod prompt;
pub mod runtime_injection;
pub mod selection;
pub mod state_capsule;
pub mod token_scope;
pub mod trigger;
pub mod types;
pub mod verifier;

use crate::context::assembly::CompactionAssembly;
use crate::context::prompt::{build_compaction_user_prompt, extract_summary, COMPACTION_SYSTEM_PROMPT};
use crate::context::selection::select_items_to_compact;
use crate::context::state_capsule::StateCapsule;
use crate::context::trigger::CompactionTrigger;
use crate::context::types::{CompactionPlan, CompactionResult};
use grodex_core::context::ContextItem;

/// Manages context compaction for a session.
///
/// Created once per session. Tracks token usage and triggers
/// compaction when the context approaches the model's window limit.
#[derive(Debug, Clone)]
pub struct CompactionManager {
    pub trigger: CompactionTrigger,
    context_window: u64,
    /// Number of times compaction has been performed this session.
    compaction_count: u64,
    /// Whether auto-compaction is currently suppressed (e.g. after a recent failure).
    suppressed: bool,
}

impl CompactionManager {
    /// Create a new compaction manager.
    pub fn new(context_window: u64) -> Self {
        Self {
            trigger: CompactionTrigger::default(),
            context_window,
            compaction_count: 0,
            suppressed: false,
        }
    }

    /// Set a custom trigger configuration.
    pub fn with_trigger(mut self, trigger: CompactionTrigger) -> Self {
        self.trigger = trigger;
        self
    }

    /// Update the context window size (e.g. after model switch).
    pub fn set_context_window(&mut self, window: u64) {
        self.context_window = window;
    }

    /// Update the trigger threshold (percentage of the context window at
    /// which auto-compaction fires). Clamped to 1..=100. Configurable via
    /// `compaction_threshold_percent` in config (default 85).
    pub fn set_threshold_percent(&mut self, percent: u8) {
        self.trigger.threshold_percent = percent.clamp(1, 100);
    }

    /// Whether compaction should be triggered now.
    pub fn should_compact(&self, current_tokens: u64) -> bool {
        if self.suppressed {
            return false;
        }
        self.trigger
            .should_compact(current_tokens, self.context_window)
    }

    /// Whether the current usage would overflow the window.
    pub fn is_overflow(&self, current_tokens: u64) -> bool {
        self.trigger
            .is_preflight_overflow(current_tokens, self.context_window)
    }

    /// Suppress auto-compaction (e.g. after a compaction failure).
    pub fn suppress(&mut self) {
        self.suppressed = true;
    }

    /// Re-enable auto-compaction.
    pub fn unsuppress(&mut self) {
        self.suppressed = false;
    }

    /// Build a compaction plan from the current context items.
    ///
    /// Reserves ~20% of the window for the model response and overhead.
    /// Returns None if compaction isn't worthwhile.
    pub fn plan_compaction(&self, items: &[ContextItem]) -> Option<CompactionPlan> {
        let target_keep = self.context_window / 5; // keep ~20% of window as recent
        select_items_to_compact(items, target_keep, self.trigger.min_compactable_tokens)
    }

    /// Build the system and user prompts for the compaction model call.
    pub fn build_compaction_prompt(plan: &CompactionPlan) -> (String, String) {
        let system = COMPACTION_SYSTEM_PROMPT.to_string();
        let user = build_compaction_user_prompt(&plan.format_for_prompt());
        (system, user)
    }

    /// Extract and validate the summary from a model response.
    pub fn process_summary(&mut self, response: &str, plan: &CompactionPlan) -> CompactionResult {
        self.compaction_count += 1;
        let summary = extract_summary(response);
        let tokens_after = (summary.len() as u64).div_ceil(4) + plan.keep_tokens;

        CompactionResult {
            summary,
            tokens_before: plan.estimated_tokens_before,
            tokens_after,
            reduction_ratio: if plan.estimated_tokens_before > 0 {
                tokens_after as f64 / plan.estimated_tokens_before as f64
            } else {
                1.0
            },
        }
    }

    /// Rebuild the context projection after compaction.
    pub fn rebuild_context(
        preserved: Vec<ContextItem>,
        result: &CompactionResult,
        capsule: &StateCapsule,
        recent: Vec<ContextItem>,
    ) -> Vec<ContextItem> {
        CompactionAssembly::assemble(preserved, result.summary.clone(), capsule, recent)
    }

    /// Validate the compacted context.
    pub fn validate_context(items: &[ContextItem]) -> Vec<String> {
        CompactionAssembly::validate(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_compaction_needed() {
        let mgr = CompactionManager::new(100_000);
        assert!(mgr.should_compact(90_000)); // 90% > 85% threshold
        assert!(!mgr.should_compact(50_000));
    }

    #[test]
    fn suppression_blocks_compaction() {
        let mut mgr = CompactionManager::new(100_000);
        mgr.suppress();
        assert!(!mgr.should_compact(90_000));
        mgr.unsuppress();
        assert!(mgr.should_compact(90_000));
    }

    #[test]
    fn overflow_detection() {
        let mgr = CompactionManager::new(100_000);
        assert!(mgr.is_overflow(101_000));
        assert!(!mgr.is_overflow(99_000));
    }

    #[test]
    fn plan_compaction_splits_items() {
        let mut mgr = CompactionManager::new(5000);
        mgr.trigger.min_compactable_tokens = 20;
        let items: Vec<ContextItem> = (0..100)
            .map(|i| ContextItem::User {
                content: format!("long message number {i} with lots of extra text padding to build up enough token count for compaction testing purposes xxxxxxxxxxxxxxxxx"),
                message_id: None,
            })
            .collect();

        let plan = mgr.plan_compaction(&items);
        assert!(plan.is_some(), "plan should exist when total tokens > target_keep + min");
        let plan = plan.unwrap();
        assert!(!plan.items_to_keep.is_empty());
    }
}
