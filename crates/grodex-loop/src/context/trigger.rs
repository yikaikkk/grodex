//! Compaction trigger — token budget detection and auto-trigger logic.
//!
//! Following Grok's `exceeds_threshold()` pattern: integer-arithmetic
//! threshold check. Triggers at 85% by default.

/// Detects when compaction should be triggered based on token usage.
#[derive(Debug, Clone)]
pub struct CompactionTrigger {
    /// Percentage of context window at which auto-compaction fires (default 85).
    pub threshold_percent: u8,
    /// Minimum tokens worth of content before compaction is worthwhile (default 5000).
    pub min_compactable_tokens: u64,
    /// Whether auto-compaction is enabled.
    pub enabled: bool,
}

impl Default for CompactionTrigger {
    fn default() -> Self {
        Self {
            threshold_percent: 85,
            min_compactable_tokens: 5000,
            enabled: true,
        }
    }
}

impl CompactionTrigger {
    /// Whether auto-compaction should fire given current usage and window size.
    ///
    /// Uses integer math to avoid floating-point: `used * 100 >= window * threshold`.
    pub fn should_compact(&self, used_tokens: u64, context_window: u64) -> bool {
        if !self.enabled {
            return false;
        }
        if used_tokens < self.min_compactable_tokens {
            return false;
        }
        if context_window == 0 {
            return false;
        }
        // Integer threshold check: used / window >= threshold / 100
        used_tokens
            .saturating_mul(100)
            >= context_window.saturating_mul(self.threshold_percent as u64)
    }

    /// Whether the estimated usage exceeds the context window entirely.
    /// This is more urgent than threshold-based compaction — it's an overflow.
    pub fn is_preflight_overflow(&self, used_tokens: u64, context_window: u64) -> bool {
        if context_window == 0 {
            return false;
        }
        used_tokens > context_window
    }

    /// How close we are to the threshold (0.0 = empty, 1.0 = at threshold).
    pub fn usage_ratio(&self, used_tokens: u64, context_window: u64) -> f64 {
        if context_window == 0 {
            return 0.0;
        }
        used_tokens as f64 / context_window as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_at_85_percent() {
        let trigger = CompactionTrigger::default();
        // 85% of 100K = 85K
        assert!(trigger.should_compact(85_000, 100_000));
        assert!(!trigger.should_compact(84_999, 100_000));
    }

    #[test]
    fn respects_min_compactable() {
        let trigger = CompactionTrigger::default();
        // 85% of 5000 = 4250, but below min_compactable (5000)
        assert!(!trigger.should_compact(4_250, 5_000));
        // Above min_compactable and over threshold
        let trigger2 = CompactionTrigger {
            min_compactable_tokens: 500,
            ..Default::default()
        };
        assert!(trigger2.should_compact(900, 1_000)); // 90% > 85%, and 900 > 500
    }

    #[test]
    fn preflight_overflow() {
        let trigger = CompactionTrigger::default();
        assert!(trigger.is_preflight_overflow(101_000, 100_000));
        assert!(!trigger.is_preflight_overflow(99_000, 100_000));
    }

    #[test]
    fn disabled_trigger_never_fires() {
        let trigger = CompactionTrigger {
            enabled: false,
            ..Default::default()
        };
        assert!(!trigger.should_compact(u64::MAX, 1_000));
    }
}
