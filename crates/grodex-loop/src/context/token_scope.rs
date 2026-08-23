//! Auto-compact token scope accounting (Doc 11 §5.4, Phase 3).
//!
//! The compaction trigger limit is NOT a single fixed percentage — it is
//! supplied by model + config:
//!
//! ```text
//! model_auto_compact_token_limit
//! model_auto_compact_token_limit_scope   (Total | BodyAfterPrefix)
//! full_context_window
//! auto_compact_fallback_buffer_tokens
//! ```
//!
//! With scope `Total` the whole active context counts; with
//! `BodyAfterPrefix` only tokens ADDED AFTER the stable prefix count
//! toward the limit — a Turn whose prefix already consumes most of the
//! window therefore keeps compacting based on genuine growth, not on the
//! cached prefix it cannot shed anyway.
//!
//! When `model_auto_compact_token_limit` is unset the effective limit
//! falls back to `full_context_window - auto_compact_fallback_buffer_tokens`.
//! A model downshift to a smaller window clamps the limit down — the
//! limit can never exceed the window it applies to.

use serde::{Deserialize, Serialize};

/// Which part of the context counts toward the auto-compact limit
/// (Doc 11 §5.4 `model_auto_compact_token_limit_scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    /// Count the entire active context.
    Total,
    /// Count only the body added after the Turn's stable prefix.
    BodyAfterPrefix,
}

/// The four knobs governing auto-compaction triggering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoCompactLimits {
    /// Explicit per-model limit; `None` → fall back to window − buffer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_auto_compact_token_limit: Option<u64>,
    pub scope: TokenScope,
    pub full_context_window: u64,
    /// Reserved headroom used when no explicit limit is configured.
    pub auto_compact_fallback_buffer_tokens: u64,
}

impl AutoCompactLimits {
    /// The limit that actually applies: the explicit model limit (if any)
    /// clamped to the window, else window − fallback buffer. Never zero
    /// unless the window itself is zero.
    pub fn effective_limit(&self) -> u64 {
        match self.model_auto_compact_token_limit {
            Some(limit) => limit.min(self.full_context_window),
            None => self.full_context_window.saturating_sub(self.auto_compact_fallback_buffer_tokens),
        }
    }

    /// Model downshift: shrink to a smaller context window. The explicit
    /// limit is kept but re-clamped — a limit larger than the new window
    /// would be unreachable and thus silently disable compaction.
    pub fn downshift_window(&mut self, new_window: u64) {
        self.full_context_window = new_window;
    }
}

/// Stable-prefix bookkeeping for one Turn.
///
/// The prefix is the leading portion of the context that stays
/// byte-identical across Steps within the Turn (the part Prompt Cache
/// keys on). Its token count is captured once and never shrinks during
/// the Turn — Doc 11 §10 "leading context stable by default".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StablePrefixTokens(u64);

impl StablePrefixTokens {
    /// Capture the prefix size at Turn start (or after a compaction
    /// rebuild installs a new stable prefix).
    pub fn capture(tokens: u64) -> Self {
        Self(tokens)
    }

    pub fn tokens(self) -> u64 {
        self.0
    }

    /// Tokens added after the prefix, saturating at zero — a total that
    /// somehow reports below the prefix (stale estimator) must not
    /// produce a negative body.
    pub fn body_after(self, total_tokens: u64) -> u64 {
        total_tokens.saturating_sub(self.0)
    }
}

/// Stateless scope accountant: answers "how many tokens count" and
/// "should we compact now" for a given configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenAccountant;

impl TokenAccountant {
    /// Count the tokens that matter under `scope`.
    pub fn count(scope: TokenScope, total_tokens: u64, prefix: StablePrefixTokens) -> u64 {
        match scope {
            TokenScope::Total => total_tokens,
            TokenScope::BodyAfterPrefix => prefix.body_after(total_tokens),
        }
    }

    /// Whether auto-compaction should fire at the trigger point. The
    /// counted scope total at or above the effective limit fires
    /// (inclusive — the limit is the last tolerable value, and the NEXT
    /// sample would overflow).
    pub fn should_compact(
        limits: &AutoCompactLimits,
        total_tokens: u64,
        prefix: StablePrefixTokens,
    ) -> bool {
        Self::count(limits.scope, total_tokens, prefix) >= limits.effective_limit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(scope: TokenScope, limit: Option<u64>, window: u64, buffer: u64) -> AutoCompactLimits {
        AutoCompactLimits {
            model_auto_compact_token_limit: limit,
            scope,
            full_context_window: window,
            auto_compact_fallback_buffer_tokens: buffer,
        }
    }

    #[test]
    fn explicit_limit_wins_over_fallback() {
        let l = limits(TokenScope::Total, Some(50_000), 200_000, 20_000);
        assert_eq!(l.effective_limit(), 50_000);
    }

    #[test]
    fn fallback_is_window_minus_buffer() {
        let l = limits(TokenScope::Total, None, 200_000, 20_000);
        assert_eq!(l.effective_limit(), 180_000);
    }

    #[test]
    fn explicit_limit_is_clamped_to_the_window() {
        // A limit above the window would be unreachable; clamp it.
        let l = limits(TokenScope::Total, Some(500_000), 200_000, 20_000);
        assert_eq!(l.effective_limit(), 200_000);
    }

    #[test]
    fn total_scope_counts_everything() {
        let prefix = StablePrefixTokens::capture(60_000);
        assert_eq!(TokenAccountant::count(TokenScope::Total, 90_000, prefix), 90_000);
    }

    #[test]
    fn body_after_prefix_counts_only_growth() {
        let prefix = StablePrefixTokens::capture(60_000);
        assert_eq!(TokenAccountant::count(TokenScope::BodyAfterPrefix, 90_000, prefix), 30_000);
        // Stale total below the prefix saturates at zero, never negative.
        assert_eq!(TokenAccountant::count(TokenScope::BodyAfterPrefix, 50_000, prefix), 0);
    }

    #[test]
    fn body_scope_defers_compaction_vs_total_scope() {
        // Same usage, two scopes: with a large stable prefix the body
        // scope stays quiet while total scope fires.
        let prefix = StablePrefixTokens::capture(80_000);
        let total_cfg = limits(TokenScope::Total, Some(95_000), 200_000, 20_000);
        let body_cfg = limits(TokenScope::BodyAfterPrefix, Some(95_000), 200_000, 20_000);
        assert!(TokenAccountant::should_compact(&total_cfg, 100_000, prefix));
        assert!(!TokenAccountant::should_compact(&body_cfg, 100_000, prefix)); // body = 20k
        // Growth past the limit fires even with a big prefix.
        assert!(TokenAccountant::should_compact(&body_cfg, 180_000, prefix)); // body = 100k
    }

    #[test]
    fn limit_boundary_is_inclusive() {
        let cfg = limits(TokenScope::Total, Some(95_000), 200_000, 20_000);
        let prefix = StablePrefixTokens::capture(0);
        assert!(!TokenAccountant::should_compact(&cfg, 94_999, prefix));
        assert!(TokenAccountant::should_compact(&cfg, 95_000, prefix));
    }

    #[test]
    fn model_downshift_clamps_the_limit() {
        // Downshift to a smaller window: an explicit limit above the new
        // window becomes the window itself; fallback shrinks too.
        let mut cfg = limits(TokenScope::Total, Some(150_000), 200_000, 20_000);
        cfg.downshift_window(100_000);
        assert_eq!(cfg.effective_limit(), 100_000);

        let mut cfg = limits(TokenScope::Total, None, 200_000, 20_000);
        cfg.downshift_window(100_000);
        assert_eq!(cfg.effective_limit(), 80_000);
    }

    #[test]
    fn prefix_capture_round_trips_through_serde() {
        let prefix = StablePrefixTokens::capture(12_345);
        let json = serde_json::to_string(&prefix).unwrap();
        let back: StablePrefixTokens = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tokens(), 12_345);
        let scope_json = serde_json::to_string(&TokenScope::BodyAfterPrefix).unwrap();
        assert_eq!(scope_json, "\"body_after_prefix\"");
    }
}
