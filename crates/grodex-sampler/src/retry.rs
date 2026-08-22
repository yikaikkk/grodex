//! Retry decision engine — pure classification + jittered exponential backoff.
//!
//! Following Grok's `retry.rs`: `classify_error()` is a pure function that
//! maps `(error, attempt, budget, progress)` → `RetryDecision`. The actor
//! loop performs the side effects (sleep, rebuild client, auth refresh).
//!
//! Backoff: 2s × 2^(attempt−1), capped at 30s, ±20% jitter via per-thread hash.

use crate::error::{EmptyResponseContext, SamplingError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ── Constants ──────────────────────────────────────────────────────

/// Default max retries.
pub const DEFAULT_MAX_RETRIES: u32 = 5;
/// Cap on consecutive 429 retries before escalating (Grok pattern).
pub const RATE_LIMIT_RETRY_THRESHOLD: u32 = 2;
/// Base backoff in milliseconds.
const BASE_BACKOFF_MS: u64 = 2000;
/// Maximum backoff in milliseconds.
const MAX_BACKOFF_MS: u64 = 30_000;

/// Global monotonic counter for jitter de-correlation.
static JITTER_SEQ: AtomicU64 = AtomicU64::new(0);

// ── Retry Budget ───────────────────────────────────────────────────

/// Per-request retry budget.
#[derive(Debug, Clone)]
pub struct RetryBudget {
    pub max_attempts: u32,
    pub max_elapsed: Duration,
    pub max_auth_refreshes: u32,
    pub rate_limit_threshold: u32,
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_RETRIES,
            max_elapsed: Duration::from_secs(120),
            max_auth_refreshes: 2,
            rate_limit_threshold: RATE_LIMIT_RETRY_THRESHOLD,
        }
    }
}

impl RetryBudget {
    /// Create a conservative budget suitable for sub-agents.
    pub fn subagent() -> Self {
        Self {
            max_attempts: 2,
            max_elapsed: Duration::from_secs(60),
            max_auth_refreshes: 1,
            rate_limit_threshold: 1,
        }
    }
}

// ── Stream Progress ────────────────────────────────────────────────

/// Tracks what semantic content has been received from the stream.
/// Used for the semantic commit fence: after crossing, we cannot
/// transparently failover or retry without aborting the Turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamProgress {
    pub text_received: bool,
    pub tool_call_started: bool,
    pub response_metadata_received: bool,
}

impl StreamProgress {
    /// Whether the semantic commit fence has been crossed.
    /// Once crossed, the response has produced visible content that
    /// cannot be silently replaced by another model.
    pub fn has_crossed_semantic_fence(&self) -> bool {
        self.text_received || self.tool_call_started
    }
}

// ── Retry Decision ─────────────────────────────────────────────────

/// The pure decision from `classify_error`. The actor loop executes
/// the side effects.
#[derive(Debug)]
pub enum RetryDecision {
    /// Retry after the given backoff.
    Retry { backoff: Duration },
    /// Retry after rebuilding the HTTP client (HTTP/1.1 fallback).
    RetryWithClientRebuild { backoff: Duration },
    /// Failover to the next candidate in the ModelRoute.
    /// Only valid before the semantic commit fence.
    FailoverToNextCandidate,
    /// Terminal error — no further retries.
    Fatal(SamplingError),
}

// ── Classification ─────────────────────────────────────────────────

/// Pure function: classify an error into a retry decision.
///
/// Decision order is load-bearing (matches Grok's `classify_error`):
///   1. Auth → Fatal (let caller handle token refresh)
///   2. Client error (4xx, serialization) → Fatal
///   3. Rate limited → Retry if under threshold, else Fatal
///   4. Retryable (5xx, Transport) → Retry if under budget, else Failover
///   5. Everything else → Fatal
pub fn classify_error(
    err: &SamplingError,
    attempt: u32,
    budget: &RetryBudget,
    progress: StreamProgress,
) -> RetryDecision {
    // Step 1: Explicitly non-retryable errors.
    if err.is_auth_error() {
        return RetryDecision::Fatal(err.clone_error());
    }

    if err.is_client_error() && !err.is_retryable() {
        return RetryDecision::Fatal(err.clone_error());
    }

    if err.is_context_length_error() {
        return RetryDecision::Fatal(err.clone_error());
    }

    // Step 2: Server said don't retry.
    if err.should_retry_header() == Some(false) {
        return RetryDecision::Fatal(err.clone_error());
    }

    // Step 2.5: Semantic commit fence. Once any semantic content (text /
    // tool-call start) has been streamed to the caller, a retry or
    // failover would duplicate or silently replace already-visible
    // output. Fail fast — Turn-level recovery decides what to do with
    // the partial content.
    if progress.has_crossed_semantic_fence() {
        return RetryDecision::Fatal(err.clone_error());
    }

    // Step 3: Check retry budget.
    if budget.max_attempts == 0 || attempt >= budget.max_attempts {
        // Budget exhausted.
        if err.is_failover_eligible() && !progress.has_crossed_semantic_fence() {
            return RetryDecision::FailoverToNextCandidate;
        }
        return RetryDecision::Fatal(err.clone_error());
    }

    // Step 4: Rate limited — check threshold.
    if err.is_rate_limited() {
        let threshold = budget.rate_limit_threshold.max(1);
        if attempt >= threshold {
            return RetryDecision::Fatal(err.clone_error());
        }
        let backoff = err.retry_after().unwrap_or_else(|| retry_backoff(attempt));
        return RetryDecision::Retry { backoff };
    }

    // Step 5: Generic retryable.
    if err.is_retryable() {
        let backoff = err.retry_after().unwrap_or_else(|| retry_backoff(attempt));

        // First transport retry: rebuild client (HTTP/1.1 fallback,
        // mirroring Grok's RetryWithClientRebuild).
        if attempt == 0 && matches!(err, SamplingError::Transport { .. }) {
            return RetryDecision::RetryWithClientRebuild { backoff };
        }

        return RetryDecision::Retry { backoff };
    }

    // Step 6: Everything else is fatal.
    RetryDecision::Fatal(err.clone_error())
}

// ── Backoff ────────────────────────────────────────────────────────

/// Jittered exponential backoff: base=2s, cap=30s, ±20% jitter.
///
/// `attempt` is 0-indexed (0 = first retry = 2s base).
/// Following Grok: `shift = attempt`, `base_ms = 2000 << shift`.
pub fn retry_backoff(attempt: u32) -> Duration {
    let shift = attempt.min(10); // 2^10 * 2000 = 2,048,000ms, far beyond cap
    let base_ms = (BASE_BACKOFF_MS)
        .checked_shl(shift)
        .unwrap_or(u64::MAX)
        .min(MAX_BACKOFF_MS);

    // ±20% jitter, de-correlated via global atomic + thread id.
    let jitter_range = base_ms / 5;
    let seq = JITTER_SEQ.fetch_add(1, Ordering::Relaxed);
    let hash = jitter_hash(seq);
    let jitter = hash % (jitter_range * 2 + 1);
    let ms = base_ms.saturating_sub(jitter_range).saturating_add(jitter);

    Duration::from_millis(ms)
}

/// Simple hash for jitter de-correlation.
fn jitter_hash(seq: u64) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    seq.hash(&mut hasher);
    // Use a per-call variation via the seq counter itself.
    hasher.write_u64(seq.wrapping_mul(6364136223846793005));
    hasher.finish()
}

// ── Cancellation-aware sleep ───────────────────────────────────────

/// Sleep for `duration` or until `cancel_token` fires.
/// Returns `true` if sleep completed, `false` if cancelled.
pub async fn sleep_or_cancel(duration: Duration, cancel_token: &tokio_util::sync::CancellationToken) -> bool {
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => false,
        _ = tokio::time::sleep(duration) => true,
    }
}

// ── Error cloning ──────────────────────────────────────────────────

impl SamplingError {
    /// Clone the error. Because `Transport` may wrap non-Clone types,
    /// this reconstructs an equivalent owned error.
    pub fn clone_error(&self) -> Self {
        match self {
            Self::Auth { message, status_code } => Self::Auth {
                message: message.clone(),
                status_code: *status_code,
            },
            Self::Transport { message, source } => Self::Transport {
                message: message.clone(),
                source: *source,
            },
            Self::Api {
                status,
                message,
                retry_after_secs,
                should_retry,
            } => Self::Api {
                status: *status,
                message: message.clone(),
                retry_after_secs: *retry_after_secs,
                should_retry: *should_retry,
            },
            Self::Serialization { message } => Self::Serialization {
                message: message.clone(),
            },
            Self::RateLimited {
                retry_after_secs,
                message,
            } => Self::RateLimited {
                retry_after_secs: *retry_after_secs,
                message: message.clone(),
            },
            Self::IdleTimeout { elapsed_secs } => Self::IdleTimeout {
                elapsed_secs: *elapsed_secs,
            },
            Self::EmptyResponse { .. } => Self::EmptyResponse {
                context: EmptyResponseContext {
                    reason: "cloned".into(),
                    had_reasoning: false,
                    content_len: 0,
                    tool_call_count: 0,
                    finish_reason: None,
                    total_tokens: None,
                },
            },
            Self::MaxTokensTruncation => Self::MaxTokensTruncation,
            Self::IncompleteToolCall { call_id } => Self::IncompleteToolCall {
                call_id: call_id.clone(),
            },
            Self::ContextLengthExceeded { message } => Self::ContextLengthExceeded {
                message: message.clone(),
            },
            Self::Internal { message } => Self::Internal {
                message: message.clone(),
            },
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TransportSource;

    fn progress_none() -> StreamProgress {
        StreamProgress::default()
    }

    fn progress_fenced() -> StreamProgress {
        StreamProgress {
            text_received: true,
            ..Default::default()
        }
    }

    #[test]
    fn first_transport_retry_rebuilds_client() {
        let err = SamplingError::transport("connect", Some(TransportSource::Connect));
        let decision = classify_error(&err, 0, &RetryBudget::default(), progress_none());
        assert!(matches!(decision, RetryDecision::RetryWithClientRebuild { .. }));
    }

    #[test]
    fn second_transport_retry_is_plain() {
        let err = SamplingError::transport("connect", Some(TransportSource::Connect));
        let decision = classify_error(&err, 1, &RetryBudget::default(), progress_none());
        assert!(matches!(decision, RetryDecision::Retry { .. }));
    }

    #[test]
    fn auth_is_always_fatal() {
        let err = SamplingError::auth(Some(401), "bad key");
        let decision = classify_error(&err, 0, &RetryBudget::default(), progress_none());
        assert!(matches!(decision, RetryDecision::Fatal(_)));
    }

    #[test]
    fn budget_exhausted_with_failover() {
        let budget = RetryBudget {
            max_attempts: 0,
            ..Default::default()
        };
        let err = SamplingError::transport("timeout", None);
        let decision = classify_error(&err, 0, &budget, progress_none());
        assert!(matches!(decision, RetryDecision::FailoverToNextCandidate));
    }

    #[test]
    fn budget_exhausted_after_fence_is_fatal() {
        let budget = RetryBudget {
            max_attempts: 0,
            ..Default::default()
        };
        let err = SamplingError::transport("timeout", None);
        let decision = classify_error(&err, 0, &budget, progress_fenced());
        assert!(matches!(decision, RetryDecision::Fatal(_)));
    }

    #[test]
    fn rate_limit_exceeds_threshold_is_fatal() {
        let budget = RetryBudget {
            max_attempts: 5,
            rate_limit_threshold: 1,
            ..Default::default()
        };
        let err = SamplingError::rate_limited(Some(30), "slow down");
        let decision = classify_error(&err, 1, &budget, progress_none());
        assert!(matches!(decision, RetryDecision::Fatal(_)));
    }

    #[test]
    fn should_retry_false_is_fatal() {
        let err = SamplingError::api(500, "error", None, Some(false));
        let decision = classify_error(&err, 0, &RetryBudget::default(), progress_none());
        assert!(matches!(decision, RetryDecision::Fatal(_)));
    }

    #[test]
    fn backoff_ranges() {
        // Attempt 0: 2000ms ± 20% → [1600, 2400]
        let d = retry_backoff(0);
        let ms = d.as_millis() as u64;
        assert!(ms >= 1600 && ms <= 2400, "got {ms}ms");

        // Attempt 5: 2000 * 32 = 64000ms → capped at 30000ms
        let d = retry_backoff(5);
        let ms = d.as_millis() as u64;
        assert!(ms >= 24000 && ms <= 36000, "got {ms}ms");

        // Attempt 10: capped at 30000ms
        let d = retry_backoff(10);
        let ms = d.as_millis() as u64;
        assert!(ms >= 24000 && ms <= 36000, "got {ms}ms");
    }

    #[test]
    fn retryable_error_after_fence_is_fatal() {
        // After the semantic fence, even normally-retryable transport
        // errors must not be retried (would duplicate streamed content).
        let err = SamplingError::transport("stream cut", Some(TransportSource::Connect));
        let decision = classify_error(&err, 0, &RetryBudget::default(), progress_fenced());
        assert!(matches!(decision, RetryDecision::Fatal(_)));
    }

    #[test]
    fn rate_limit_after_fence_is_fatal() {
        let err = SamplingError::rate_limited(Some(5), "slow down");
        let decision = classify_error(&err, 0, &RetryBudget::default(), progress_fenced());
        assert!(matches!(decision, RetryDecision::Fatal(_)));
    }

    #[test]
    fn stream_progress_fence() {
        let mut p = StreamProgress::default();
        assert!(!p.has_crossed_semantic_fence());

        p.text_received = true;
        assert!(p.has_crossed_semantic_fence());

        let mut p2 = StreamProgress::default();
        p2.tool_call_started = true;
        assert!(p2.has_crossed_semantic_fence());
    }
}
