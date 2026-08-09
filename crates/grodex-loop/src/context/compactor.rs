//! Compactor + Summarizer + Checkpointer primitives (Doc 11 §4–8).
//!
//! Design Doc 11 §4 defines compaction as a three-phase operation:
//!
//! ```text
//! 1. plan(context, budget)  -> CompactPlan      (no side effects)
//! 2. prepare(plan)          -> CompactPrepared  (compute summaries,
//!                                                     build state capsule,
//!                                                     write checkpoint)
//! 3. apply(prepared)        -> CompactResult    (swap context items,
//!                                                     emit rollout event,
//!                                                     stamp new generation)
//! ```
//!
//! A CompactPrepared can be serialized and replayed identically on
//! another host (same items, same new generation) — this is the
//! reproducibility property required for crash recovery (§9) and for
//! Eval harnesses that replay traces against different compaction
//! policies.
//!
//! §5 defines a state capsule — structured non-conversation state.
//! §6 defines checkpoint metadata stored in rollout for reconstruction.
//! Both traits are declared here alongside the Compactor trait so the
//! loop crate can pick whichever concrete implementation it wants
//! (naive head-keep, LLM summarizer, semantic chunking, …).

use grodex_core::context::ContextItem;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::state_capsule::StateCapsule;

// ── Token accounting helpers (§6 + §4 budgets) ───────────────────────────

/// How many tokens a particular context subtree consumes.
///
/// Implementations range from `char_len/4` heuristics to full tiktoken.
/// The Compactor only depends on the trait, not the specific estimator,
/// so we can swap between them for local-dev speed vs production fidelity.
pub trait TokenBudgetEstimator {
    /// Estimate tokens for one ContextItem.
    fn estimate_item(&self, item: &ContextItem) -> u64;
    /// Estimate total tokens for a slice of items. Default impl sums
    /// the per-item estimates; callers can override if a tokenizer can
    /// estimate the whole block more accurately.
    fn estimate_slice(&self, items: &[ContextItem]) -> u64 {
        items.iter().map(|i| self.estimate_item(i)).sum()
    }
}

/// A cheap heuristic estimator (`total_chars/4 + structural_overhead`)
/// suitable for dev builds and for setting the compaction trigger.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicTokenEstimator;

impl TokenBudgetEstimator for HeuristicTokenEstimator {
    fn estimate_item(&self, item: &ContextItem) -> u64 {
        let structural = 8u64; // per-item overhead (roles, separators)
        let content_chars = match item {
            ContextItem::System { content } => content.chars().count() as u64,
            ContextItem::Developer { content } => content.chars().count() as u64,
            ContextItem::User { content, .. } => content.chars().count() as u64,
            ContextItem::Assistant { content } => content.chars().count() as u64,
            ContextItem::ToolCall { arguments, .. } => arguments.to_string().chars().count() as u64,
            ContextItem::ToolResult { content, .. } => content.chars().count() as u64,
            ContextItem::CompactionSummary { summary, .. } => summary.chars().count() as u64,
            ContextItem::ReasoningSummary { content } => content.chars().count() as u64,
            ContextItem::ImagePlaceholder { mime_type, artifact_ref } => {
                (mime_type.len() + artifact_ref.len()) as u64
            }
        };
        structural + content_chars.saturating_div(4)
    }
}

// ── Compaction plan (stage 1) ────────────────────────────────────────────

/// High-level strategy used for this compaction pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactStrategy {
    /// Keep Level 0 preserved verbatim, replace middle with a single
    /// summarizer-generated block, keep tail.
    SummarizeMiddle,
    /// Keep only N most-recent user/assistant turns verbatim; everything
    /// older is discarded or collapsed into metadata.
    RecentOnly {
        turns_to_keep: usize,
    },
    /// Keep head (first-turn context) + tail (last K items) exactly,
    /// replace the middle with a high-fidelity bullet-point summary.
    HeadSummaryTail {
        head_items: usize,
        tail_items: usize,
    },
    /// Semantic: cluster recent conversation by topic, keep only cluster
    /// representatives plus full-fidelity boundary turns.
    SemanticChunk,
}

impl Default for CompactStrategy {
    fn default() -> Self {
        // Default to the strategy that behaves most predictably for
        // the "default" compaction trigger in Doc 11 §4 — cheap and
        // lossy-but-safe.
        CompactStrategy::HeadSummaryTail { head_items: 6, tail_items: 20 }
    }
}

/// Side-effect-free plan produced by `Compactor::plan`. Everything the
/// next two stages need to know is captured here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactPlan {
    /// Which strategy this plan will apply.
    pub strategy: CompactStrategy,
    /// Total tokens in the projection BEFORE compaction.
    pub input_tokens: u64,
    /// Token budget after compaction. New items' estimated token count
    /// must be <= `output_token_budget` or the plan must be rebuilt.
    pub output_token_budget: u64,
    /// Indexes (0-based in `for_model()`) of items that must be preserved
    /// verbatim — Zone A system instructions, pinned Developer rules, the
    /// on-going Turn, uncommitted tool results.
    pub preserve_indexes: Vec<usize>,
    /// First inclusive index that compaction is allowed to touch.
    pub compactable_range_start: usize,
    /// Last exclusive index that compaction may mutate.
    pub compactable_range_end: usize,
    /// If non-empty, a plan-level optional reference to a prior
    /// checkpoint the compactor can build on top of (incremental mode).
    pub from_checkpoint_id: Option<String>,
    /// Maintenance policy version stamp (for cache invalidation).
    pub maintenance_policy_version: u64,
    /// Caller-visible reason the compaction fired ("token budget exceeded"
    /// / "explicit user request" / "turn boundary"). For diagnostics.
    pub reason: String,
}

// ── Summarizer trait (stage 2 uses) ──────────────────────────────────────

/// Generates summaries from windows of ContextItem. Used by the middle
/// phase of compaction to produce the compressed replacement block.
///
/// Implementations range from a trivial bullet-list extractor (no model)
/// to an LLM-based summarizer that re-reads the full window and writes
/// a structured narrative.
pub trait Summarizer {
    /// Error type returned when summarization fails.
    type Error;
    /// Produce a summary ContextItem (usually Developer-typed, so the
    /// transcript stays honest about "this is the compressor speaking")
    /// for the given window of items.
    fn summarize(&self, window: &[ContextItem]) -> Result<Vec<ContextItem>, Self::Error>;
}

/// A no-model summarizer: extract the first line of every User message,
/// the last line of every Assistant message, and any non-zero exit codes.
/// Produces a single `ContextItem::Developer` bullet-point summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtractiveSummarizer;

impl Summarizer for ExtractiveSummarizer {
    type Error = std::convert::Infallible;

    fn summarize(&self, window: &[ContextItem]) -> Result<Vec<ContextItem>, Self::Error> {
        use std::fmt::Write;
        let mut out = String::from("## Compaction Summary (extractive)\n\n");
        for (idx, item) in window.iter().enumerate() {
            match item {
                ContextItem::User { content, .. } => {
                    let first_line = content.lines().next().unwrap_or("");
                    let _ = writeln!(out, "- [{idx}] User: {first_line}");
                }
                ContextItem::Assistant { content } => {
                    let last_line = content.lines().last().unwrap_or("");
                    if !last_line.is_empty() {
                        let _ = writeln!(out, "- [{idx}] Assistant …: {last_line}");
                    }
                }
                ContextItem::ToolResult { content, is_error, .. } => {
                    let snippet = content
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("(no stdout)");
                    let tag = if *is_error { "ERROR" } else { "Tool" };
                    let _ = writeln!(out, "- [{idx}] {tag}: {snippet}");
                }
                _ => { /* skip System / Developer / ToolCall in extraction */ }
            }
        }
        Ok(vec![ContextItem::Developer { content: out }])
    }
}

// ── Checkpointer trait ───────────────────────────────────────────────────

/// Produces a checkpoint in the rollout journal, returning a checkpoint
/// id that future compactions can reference via `from_checkpoint_id` to
/// apply incremental compaction (Doc 11 §5).
pub trait Checkpointer {
    type Error;
    /// Write a checkpoint containing (at minimum) the current projection
    /// items, source_seq_end, history_version, maintenance_policy_version,
    /// and an optional state-capsule blob. Returns the new checkpoint id.
    fn write_checkpoint(
        &mut self,
        items: &[ContextItem],
        metadata: &BTreeMap<String, String>,
    ) -> Result<String, Self::Error>;
}

/// In-memory checkpointer for tests. Does not survive restarts.
#[derive(Debug, Default)]
pub struct InMemoryCheckpointer {
    next_id: u64,
    /// key: checkpoint id, value: serialized debug blob string for tests
    /// to inspect.
    entries: BTreeMap<String, (Vec<ContextItem>, BTreeMap<String, String>)>,
}

impl Checkpointer for InMemoryCheckpointer {
    type Error = std::convert::Infallible;

    fn write_checkpoint(
        &mut self,
        items: &[ContextItem],
        metadata: &BTreeMap<String, String>,
    ) -> Result<String, Self::Error> {
        self.next_id += 1;
        let id = format!("cp_{:08x}", self.next_id);
        self.entries
            .insert(id.clone(), (items.to_vec(), metadata.clone()));
        Ok(id)
    }
}

// ── Prepared compaction (stage 2 output) ─────────────────────────────────

/// Ready-to-apply compaction. Contains the exact replacement item list,
/// the new StateCapsule, and the checkpoint id. Applying this is a pure
/// context swap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactPrepared {
    /// The id this prepared plan was derived from. Re-validated at apply.
    pub plan: CompactPlan,
    /// The new items the projection will hold after apply. `preserve`d
    /// items show up verbatim here in their original locations; the
    /// middle window holds summarizer output; and the tail is untouched.
    pub replacement_items: Vec<ContextItem>,
    /// Estimated token count of `replacement_items`. Apply phase will
    /// refuse if this exceeds `plan.output_token_budget`.
    pub estimated_output_tokens: u64,
    /// Checkpoint id written during prepare (or id of the pre-existing
    /// checkpoint used in incremental mode).
    pub checkpoint_id: String,
    /// The capsule that will be appended or stamped into context after
    /// the swap (Doc 11 §5).
    pub state_capsule: StateCapsule,
    /// A short id for this specific compaction operation — stored in the
    /// rollout `CompactionCommitted` payload for replay reconciliation.
    pub compaction_id: String,
    /// Version counter that will become the new ContextProjection's
    /// `state_capsule_id` (monotonic within a session).
    pub new_state_capsule_version: u64,
}

// ── Final result of apply (stage 3) ──────────────────────────────────────

/// Committed compaction result: deltas, tokens, new versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactResult {
    pub compaction_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Items removed / retained counts (for `grodex turn status` / metrics).
    pub items_before: usize,
    pub items_after: usize,
    pub items_removed: usize,
    pub items_preserved_verbatim: usize,
    pub checkpoint_id: String,
    pub new_history_version: u64,
    pub new_maintenance_policy_version: u64,
    pub new_state_capsule_id: Option<String>,
    /// If any, the bounded summary text visible to humans/UI (NOT the
    /// same as the Developer item in the projection, but a shorter
    /// echo).
    pub summary_echo: Option<String>,
}

// ── Compactor trait (core interface) ─────────────────────────────────────

/// The three-phase compactor contract (Doc 11 §4).
///
/// Generic over the concrete Summarizer and Checkpointer so different
/// implementations (heuristic dev build, full LLM+semantic prod build)
/// can be swapped without touching downstream call sites.
pub trait Compactor {
    type Summarizer: Summarizer;
    type Checkpointer: Checkpointer;
    type Error;

    /// Shared references to the inner collaborators.
    fn summarizer(&self) -> &Self::Summarizer;
    fn checkpointer_mut(&mut self) -> &mut Self::Checkpointer;
    fn estimator(&self) -> &dyn TokenBudgetEstimator;

    /// Plan which items to touch, under what budget, and for what reason.
    /// Pure — no side effects. `current_items` is `Projection::for_model()`.
    fn plan(
        &self,
        current_items: &[ContextItem],
        current_tokens: u64,
        budget_after_tokens: u64,
        reason: impl Into<String>,
    ) -> CompactPlan {
        // Default planner: apply HeadSummaryTail with a 30% tail rule.
        let total = current_items.len();
        let tail_keep = (total / 3).max(10).min(40);
        let head_keep = 8usize;
        let preserve: Vec<usize> = (0..head_keep.min(total))
            .filter(|i| matches!(current_items.get(*i),
                Some(ContextItem::System { .. }) | Some(ContextItem::Developer { .. })
            ))
            .collect();
        let (start, end) = if head_keep + tail_keep < total {
            (head_keep, total.saturating_sub(tail_keep))
        } else {
            (total, total) // nothing compactable
        };
        CompactPlan {
            strategy: CompactStrategy::HeadSummaryTail { head_items: head_keep, tail_items: tail_keep },
            input_tokens: current_tokens,
            output_token_budget: budget_after_tokens,
            preserve_indexes: preserve,
            compactable_range_start: start,
            compactable_range_end: end,
            from_checkpoint_id: None,
            maintenance_policy_version: 1,
            reason: reason.into(),
        }
    }

    /// Execute the summarizer + checkpointer and build a fully-prepared
    /// compaction. This phase can be slow (may call into an LLM) but is
    /// *still side-effect-free on the transcript projection* — the only
    /// side effect allowed is writing a checkpoint journal entry.
    fn prepare(
        &mut self,
        plan: CompactPlan,
        current_items: &[ContextItem],
    ) -> Result<CompactPrepared, Self::Error>;

    /// Swap the projection's items to `prepared.replacement_items`, bump
    /// all the version counters, write metadata, and return the committed
    /// result.
    fn apply(
        &self,
        prepared: CompactPrepared,
        current_history_version: u64,
        current_maintenance_policy_version: u64,
    ) -> Result<CompactResult, Self::Error>;
}

/// A concrete, dependency-free Compactor implementation that ships in
/// the default loop runtime. Uses `HeuristicTokenEstimator`,
/// `ExtractiveSummarizer`, and `InMemoryCheckpointer` when callers
/// don't wish to wire in heavier implementations. Suitable for local
/// dev; production can plug in a different `Compactor` impl.
pub struct DefaultCompactor<S = ExtractiveSummarizer, C = InMemoryCheckpointer, E = HeuristicTokenEstimator> {
    pub summarizer: S,
    pub checkpointer: C,
    pub estimator: E,
    /// Monotonic counter for state capsule versions (Doc 11 §5).
    pub state_capsule_version: u64,
}

impl Default for DefaultCompactor {
    fn default() -> Self {
        Self {
            summarizer: ExtractiveSummarizer,
            checkpointer: InMemoryCheckpointer::default(),
            estimator: HeuristicTokenEstimator,
            state_capsule_version: 0,
        }
    }
}

impl<S, C, E> Compactor for DefaultCompactor<S, C, E>
where
    S: Summarizer,
    C: Checkpointer,
    E: TokenBudgetEstimator,
{
    type Summarizer = S;
    type Checkpointer = C;
    type Error = CompactError<C::Error, S::Error>;

    fn summarizer(&self) -> &Self::Summarizer { &self.summarizer }
    fn checkpointer_mut(&mut self) -> &mut Self::Checkpointer { &mut self.checkpointer }
    fn estimator(&self) -> &dyn TokenBudgetEstimator { &self.estimator }

    fn prepare(
        &mut self,
        plan: CompactPlan,
        current_items: &[ContextItem],
    ) -> Result<CompactPrepared, Self::Error> {
        // 1. Nothing compactable: early return with zero-op prepared plan.
        if plan.compactable_range_start >= plan.compactable_range_end
            || plan.compactable_range_end > current_items.len()
        {
            let replacement_items = current_items.to_vec();
            let est = self.estimator.estimate_slice(&replacement_items);
            let mut cp_meta = BTreeMap::new();
            cp_meta.insert("zero-op".into(), "true".into());
            let cp_id = self
                .checkpointer
                .write_checkpoint(current_items, &cp_meta)
                .map_err(CompactError::Checkpoint)?;
            self.state_capsule_version += 1;
            return Ok(CompactPrepared {
                replacement_items,
                estimated_output_tokens: est,
                checkpoint_id: cp_id,
                state_capsule: StateCapsule::new(),
                compaction_id: format!("compact_{}", std::process::id()),
                new_state_capsule_version: self.state_capsule_version,
                plan,
            });
        }

        // 2. Split current_items into: head preserved, middle window, tail preserved.
        let (start, end) = (plan.compactable_range_start, plan.compactable_range_end);
        let head = &current_items[..start];
        let middle = &current_items[start..end];
        let tail = &current_items[end..];

        // 3. Run the summarizer on the middle window.
        let summary_items = self
            .summarizer
            .summarize(middle)
            .map_err(CompactError::Summarize)?;

        // 4. Assemble replacement: head + summary + tail.
        //    Additionally preserve any items listed in preserve_indexes
        //    that might sit inside the middle window (those are dropped
        //    from the summary and inserted back at their original spots).
        let preserved_set: std::collections::BTreeSet<usize> =
            plan.preserve_indexes.iter().copied().collect();
        let mut replacement: Vec<ContextItem> = Vec::with_capacity(
            head.len() + summary_items.len() + tail.len() + preserved_set.len(),
        );
        for (idx, item) in head.iter().enumerate() {
            replacement.push(item.clone());
            debug_assert_eq!(idx, replacement.len() - 1);
        }
        // Insert preserved items inside middle first, in original order.
        let mut preserved_in_middle: Vec<(usize, &ContextItem)> = preserved_set
            .iter()
            .filter(|i| **i >= start && **i < end)
            .map(|i| (*i, &current_items[*i]))
            .collect();
        preserved_in_middle.sort_by_key(|(i, _)| *i);
        replacement.extend(summary_items);
        for (_, item) in preserved_in_middle {
            replacement.push(item.clone());
        }
        for item in tail {
            replacement.push(item.clone());
        }

        // 5. Check budget — refuse over-budget prepare results.
        let est = self.estimator.estimate_slice(&replacement);
        if est > plan.output_token_budget {
            return Err(CompactError::OverBudget {
                estimate: est,
                budget: plan.output_token_budget,
            });
        }

        // 6. Write checkpoint with metadata mirrors of the plan.
        let mut cp_meta = BTreeMap::new();
        cp_meta.insert("strategy".into(), format!("{:?}", plan.strategy));
        cp_meta.insert("input_tokens".into(), plan.input_tokens.to_string());
        cp_meta.insert("output_tokens_estimate".into(), est.to_string());
        cp_meta.insert("reason".into(), plan.reason.clone());
        let cp_id = self
            .checkpointer
            .write_checkpoint(&replacement, &cp_meta)
            .map_err(CompactError::Checkpoint)?;

        // 7. Build a minimal state capsule carrying the maintenance
        //    policy version and the compaction echo.
        let mut capsule = StateCapsule::new();
        capsule.add_section(
            "Compaction",
            format!(
                "id={}\nstrategy={:?}\nfrom_checkpoint={}",
                "compact_prepared",
                plan.strategy,
                plan.from_checkpoint_id.as_deref().unwrap_or("none")
            ),
        );
        self.state_capsule_version += 1;

        Ok(CompactPrepared {
            plan,
            replacement_items: replacement,
            estimated_output_tokens: est,
            checkpoint_id: cp_id,
            state_capsule: capsule,
            compaction_id: format!("cpm_{}_{:08x}", std::process::id(), self.state_capsule_version),
            new_state_capsule_version: self.state_capsule_version,
        })
    }

    fn apply(
        &self,
        prepared: CompactPrepared,
        current_history_version: u64,
        current_maintenance_policy_version: u64,
    ) -> Result<CompactResult, Self::Error> {
        // Safety fence: a prepared can only be applied once. This is enforced
        // structurally by requiring `CompactPrepared` by value — once
        // consumed it cannot be re-applied. Additional safety: verify budget.
        if prepared.estimated_output_tokens > prepared.plan.output_token_budget {
            return Err(CompactError::OverBudget {
                estimate: prepared.estimated_output_tokens,
                budget: prepared.plan.output_token_budget,
            });
        }

        let _items_before = prepared.plan.input_tokens;
        let items_preserved = prepared.plan.preserve_indexes.len();
        let items_after_count = prepared.replacement_items.len();
        let items_removed = (prepared.plan.compactable_range_end - prepared.plan.compactable_range_start)
            .saturating_sub(items_preserved);

        Ok(CompactResult {
            compaction_id: prepared.compaction_id,
            input_tokens: prepared.plan.input_tokens,
            output_tokens: prepared.estimated_output_tokens,
            items_before: prepared.plan.input_tokens as usize, // legacy: keep as usize count
            items_after: items_after_count,
            items_removed,
            items_preserved_verbatim: items_preserved,
            checkpoint_id: prepared.checkpoint_id,
            new_history_version: current_history_version + 1,
            new_maintenance_policy_version: current_maintenance_policy_version
                .max(prepared.plan.maintenance_policy_version),
            new_state_capsule_id: Some(format!(
                "sc_{:08x}",
                prepared.new_state_capsule_version
            )),
            summary_echo: Some(prepared.plan.reason),
        })
    }
}

/// Unified error type for the `DefaultCompactor` pipeline.
#[derive(Debug)]
pub enum CompactError<CE, SE> {
    /// Checkpoint write failed (I/O, serialization, journal full).
    Checkpoint(CE),
    /// Summarizer failed (LLM error, bad tokens, timeout).
    Summarize(SE),
    /// Prepared output exceeded the plan's token budget. The manager
    /// should re-plan with a more aggressive strategy or a larger budget.
    OverBudget { estimate: u64, budget: u64 },
}
