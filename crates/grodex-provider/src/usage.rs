//! Token usage types — strict separation between pre-request estimates and
//! post-request settled billing.
//!
//! Estimated = computed locally BEFORE the request (for watermark, compaction).
//! Settled   = reported by provider AFTER the request (for billing, accounting).
//! These are never combined or confused.

use crate::binding::ModelBindingId;
use chrono::{DateTime, Utc};
use grodex_core::id::{SessionId, StepId, TurnId};
use serde::{Deserialize, Serialize};

// ── Raw token counts (provider-reported) ───────────────────────────

/// Normalized token usage across all wire backends.
///
/// Input tokens are FULL input (uncached + cache reads + cache writes).
/// `cached_input_tokens` is the cache-hit subset — do NOT subtract it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    /// The subset of input tokens that were cache hits.
    pub cached_input_tokens: u64,
    /// Tokens spent writing to the cache.
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

// ── Estimated (pre-request) ────────────────────────────────────────

/// Estimated usage computed BEFORE the request by a local token counter.
/// Used for context window watermark, max output limit, and compaction decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EstimatedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tool_schema_tokens: u64,
    pub wire_overhead_tokens: u64,
    pub total: u64,
    pub confidence: EstimateConfidence,
    pub tokenizer_id: Option<String>,
    pub tokenizer_version: Option<String>,
}

/// Confidence level of a token estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EstimateConfidence {
    /// Token counts come from the exact model tokenizer.
    Exact,
    /// Token counts come from a related model's tokenizer.
    ModelTokenizer,
    /// Character-based approximation (~4 bytes/token).
    Approximate,
}

impl EstimatedUsage {
    /// Create a quick approximate estimate using the bytes/4 heuristic.
    pub fn approximate(input_chars: usize) -> Self {
        let input_tokens = (input_chars as u64).max(4) / 4;
        Self {
            input_tokens,
            output_tokens: 0,
            tool_schema_tokens: 0,
            wire_overhead_tokens: 0,
            total: input_tokens,
            confidence: EstimateConfidence::Approximate,
            tokenizer_id: None,
            tokenizer_version: None,
        }
    }
}

// ── Settled (post-request) ─────────────────────────────────────────

/// Settled usage from the provider after a completed request.
///
/// This is the billing source of truth. When `estimated` is true, the provider
/// did not return usage and we fell back to the pre-request estimate — the
/// flag ensures this is never confused with authoritative billing data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettledUsage {
    /// True if the provider did NOT return usage and we fell back to estimates.
    pub estimated: bool,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    /// Cost in micro-units (e.g., micro-cents USD = 1e-8 USD).
    pub cost_micro_units: Option<i64>,
    /// ISO 4217 currency code.
    pub currency: Option<String>,
}

impl From<TokenUsage> for SettledUsage {
    fn from(u: TokenUsage) -> Self {
        Self {
            estimated: false,
            input_tokens: u.input_tokens,
            cached_input_tokens: u.cached_input_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
            output_tokens: u.output_tokens,
            reasoning_tokens: u.reasoning_tokens,
            total_tokens: u.total_tokens,
            cost_micro_units: None,
            currency: None,
        }
    }
}

// ── Usage Record (audit trail) ─────────────────────────────────────

/// Append-only usage record for the audit trail.
///
/// Written to rollout.jsonl for every model attempt (including failed ones
/// that were billed by the provider). Sub-agent usage carries a `task_run_id`
/// for tree-level accounting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub step_id: StepId,
    pub task_run_id: Option<String>,
    pub model_binding_id: ModelBindingId,
    pub attempt_number: u32,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub cost_micro_units: Option<i64>,
    pub currency: Option<String>,
    pub estimated: bool,
    pub provider_request_id: Option<String>,
    pub timestamp: DateTime<Utc>,
}
