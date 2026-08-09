//! CircuitBreaker — Closed/Open/HalfOpen state machine with sliding window.
//!
//! Following Grok's `xai-circuit-breaker` crate pattern:
//!   - Sliding window with incremental failure counter for O(1) error_rate
//!   - Atomic state machine with lock-free `is_open()` fast path
//!   - HalfOpen: one probe at a time, abandoned lease reclaim
//!   - Configurable failure codes, error rate threshold, cooldown

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// ── State ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BreakerState {
    Closed = 0,
    Open = 1,
    HalfOpen = 2,
}

/// Outcome of a single request through the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
}

/// Error returned when the breaker is open.
#[derive(Debug, Clone, thiserror::Error)]
#[error("circuit breaker open; retry after {:.1}s", retry_after.as_secs_f64())]
pub struct BreakerOpen {
    pub retry_after: Duration,
}

// ── Configuration ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BreakerConfig {
    /// Sliding window duration.
    pub window_duration: Duration,
    /// Minimum samples before tripping.
    pub min_samples: usize,
    /// Error rate threshold (0.0–1.0) to trip Open.
    pub error_rate_threshold: f64,
    /// Duration to stay Open before transitioning to HalfOpen.
    pub open_duration: Duration,
    /// Maximum concurrent probes in HalfOpen.
    pub half_open_max_probes: usize,
    /// HTTP status codes considered failures.
    pub failure_codes: Vec<u16>,
    /// Whether the breaker is enabled.
    pub enabled: bool,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            window_duration: Duration::from_secs(60),
            min_samples: 10,
            error_rate_threshold: 0.5,
            open_duration: Duration::from_secs(10),
            half_open_max_probes: 1,
            failure_codes: vec![429, 500, 502, 503, 504],
            enabled: true,
        }
    }
}

impl BreakerConfig {
    /// Check if a status code is a failure.
    pub fn is_failure_status(&self, status: u16) -> bool {
        self.failure_codes.contains(&status)
    }
}

// ── Sliding Window ─────────────────────────────────────────────────

const MAX_WINDOW_ENTRIES: usize = 10_000;

#[derive(Debug)]
struct SlidingWindow {
    entries: VecDeque<(Instant, bool)>,
    failures: usize,
}

impl SlidingWindow {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            failures: 0,
        }
    }

    fn push(&mut self, now: Instant, is_failure: bool) {
        if self.entries.len() >= MAX_WINDOW_ENTRIES {
            if let Some((_, was_failure)) = self.entries.pop_front() {
                if was_failure {
                    self.failures = self.failures.saturating_sub(1);
                }
            }
        }
        if is_failure {
            self.failures += 1;
        }
        self.entries.push_back((now, is_failure));
    }

    fn evict(&mut self, window: Duration, now: Instant) {
        let cutoff = now.checked_sub(window).unwrap_or(Instant::now());
        while let Some((ts, was_failure)) = self.entries.front() {
            if *ts >= cutoff {
                break;
            }
            if *was_failure {
                self.failures = self.failures.saturating_sub(1);
            }
            self.entries.pop_front();
        }
    }

    fn sample_count(&self) -> usize {
        self.entries.len()
    }

    fn error_rate(&self) -> f64 {
        let n = self.entries.len();
        if n == 0 { 0.0 } else { self.failures as f64 / n as f64 }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.failures = 0;
    }
}

// ── Circuit Breaker ────────────────────────────────────────────────

pub struct CircuitBreaker {
    config: BreakerConfig,
    state: AtomicU8,
    opened_at_ms: AtomicU64,
    half_open_probes: AtomicUsize,
    probe_claimed_at_ms: AtomicU64,
    is_open_fast: AtomicBool,
    window: Mutex<SlidingWindow>,
    start: Instant,
}

impl CircuitBreaker {
    /// Create a new circuit breaker.
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            state: AtomicU8::new(BreakerState::Closed as u8),
            opened_at_ms: AtomicU64::new(0),
            half_open_probes: AtomicUsize::new(0),
            probe_claimed_at_ms: AtomicU64::new(0),
            is_open_fast: AtomicBool::new(false),
            window: Mutex::new(SlidingWindow::new()),
            start: Instant::now(),
        }
    }

    /// Check if a request can proceed. Returns `Err(BreakerOpen)` if the
    /// circuit is open and the cooldown hasn't elapsed.
    pub fn check(&self) -> Result<(), BreakerOpen> {
        if !self.config.enabled {
            return Ok(());
        }

        let state = self.current_state();
        match state {
            BreakerState::Closed => Ok(()),
            BreakerState::Open => self.check_open(),
            BreakerState::HalfOpen => self.try_probe(),
        }
    }

    /// Record the outcome of a request.
    pub fn record(&self, outcome: Outcome) {
        if !self.config.enabled {
            return;
        }

        let now = self.now_ms();
        {
            let mut window = self.window.lock().unwrap();
            window.evict(self.config.window_duration, self.start + Duration::from_millis(now));
            window.push(
                self.start + Duration::from_millis(now),
                matches!(outcome, Outcome::Failure),
            );
        }

        let state = self.current_state();
        match state {
            BreakerState::Closed => self.maybe_trip(),
            BreakerState::HalfOpen => match outcome {
                Outcome::Success => self.close("probe_success"),
                Outcome::Failure => self.trip("probe_failure"),
            },
            BreakerState::Open => {
                // Continue collecting samples even when open.
            }
        }
    }

    /// Current state (lock-free read).
    pub fn current_state(&self) -> BreakerState {
        match self.state.load(Ordering::Acquire) {
            0 => BreakerState::Closed,
            1 => BreakerState::Open,
            _ => BreakerState::HalfOpen,
        }
    }

    /// Lock-free check if the breaker is currently open.
    pub fn is_open(&self) -> bool {
        self.is_open_fast.load(Ordering::Relaxed)
    }

    // ── Internal ──────────────────────────────────────────────────

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn check_open(&self) -> Result<(), BreakerOpen> {
        let now = self.now_ms();
        let opened_at = self.opened_at_ms.load(Ordering::Acquire);
        let elapsed = now.saturating_sub(opened_at);
        let open_ms = self.config.open_duration.as_millis() as u64;

        if elapsed >= open_ms {
            // Try to transition Open → HalfOpen.
            if self
                .state
                .compare_exchange(
                    BreakerState::Open as u8,
                    BreakerState::HalfOpen as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.is_open_fast.store(false, Ordering::Release);
                self.half_open_probes.store(0, Ordering::Release);
                return self.try_probe();
            }
            // CAS lost — re-evaluate.
            return self.check();
        }

        let remaining_ms = open_ms.saturating_sub(elapsed);
        Err(BreakerOpen {
            retry_after: Duration::from_millis(remaining_ms),
        })
    }

    fn try_probe(&self) -> Result<(), BreakerOpen> {
        let max = self.config.half_open_max_probes.max(1);
        let prev = self.half_open_probes.fetch_add(1, Ordering::AcqRel);

        if prev < max {
            self.probe_claimed_at_ms.store(self.now_ms(), Ordering::Release);
            return Ok(());
        }

        // Undo increment: probe slots exhausted.
        self.half_open_probes.fetch_sub(1, Ordering::Release);

        // Abandoned lease reclaim: if the current claim is older than
        // open_duration, take it over.
        let claimed = self.probe_claimed_at_ms.load(Ordering::Acquire);
        let now = self.now_ms();
        let open_ms = self.config.open_duration.as_millis() as u64;

        if now.saturating_sub(claimed) >= open_ms
            && self
                .probe_claimed_at_ms
                .compare_exchange(claimed, now, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            self.half_open_probes.fetch_add(1, Ordering::Release);
            return Ok(());
        }

        Err(BreakerOpen {
            retry_after: Duration::from_millis(50).min(self.config.open_duration),
        })
    }

    fn maybe_trip(&self) {
        let window = self.window.lock().unwrap();
        let samples = window.sample_count();
        if samples >= self.config.min_samples && window.error_rate() >= self.config.error_rate_threshold {
            drop(window);
            self.trip("error_rate_threshold");
        }
    }

    fn trip(&self, _reason: &str) {
        let prev = self.state.swap(BreakerState::Open as u8, Ordering::AcqRel);
        if prev == BreakerState::Open as u8 {
            return; // already open
        }
        self.opened_at_ms.store(self.now_ms(), Ordering::Release);
        self.half_open_probes.store(0, Ordering::Release);
        self.is_open_fast.store(true, Ordering::Release);
    }

    fn close(&self, _reason: &str) {
        self.state.store(BreakerState::Closed as u8, Ordering::Release);
        self.window.lock().unwrap().clear();
        self.half_open_probes.store(0, Ordering::Release);
        self.is_open_fast.store(false, Ordering::Release);
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_breaker_is_closed() {
        let cb = CircuitBreaker::new(BreakerConfig::default());
        assert_eq!(cb.current_state(), BreakerState::Closed);
        assert!(!cb.is_open());
        assert!(cb.check().is_ok());
    }

    #[test]
    fn disabled_breaker_always_allows() {
        let mut cfg = BreakerConfig::default();
        cfg.enabled = false;
        let cb = CircuitBreaker::new(cfg);
        assert!(cb.check().is_ok());
        cb.record(Outcome::Failure);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn trips_after_error_threshold() {
        let mut cfg = BreakerConfig::default();
        cfg.window_duration = Duration::from_secs(3600);
        cfg.min_samples = 3;
        cfg.error_rate_threshold = 0.5;
        let cb = CircuitBreaker::new(cfg);

        cb.record(Outcome::Failure);
        cb.record(Outcome::Failure);
        cb.record(Outcome::Success);
        // 2/3 failures = 0.667 > 0.5 threshold → trip
        assert!(cb.is_open());
        assert!(cb.check().is_err());
    }

    #[test]
    fn half_open_probe_succeeds_and_closes() {
        let mut cfg = BreakerConfig::default();
        cfg.window_duration = Duration::from_secs(3600);
        cfg.min_samples = 1;
        cfg.open_duration = Duration::from_millis(1);
        let cb = CircuitBreaker::new(cfg);

        // Trip the breaker.
        cb.record(Outcome::Failure);
        assert!(cb.is_open());

        // Wait for cooldown.
        std::thread::sleep(Duration::from_millis(10));
        assert!(cb.check().is_ok()); // probe admitted
        assert_eq!(cb.current_state(), BreakerState::HalfOpen);

        // Probe succeeds → close.
        cb.record(Outcome::Success);
        assert_eq!(cb.current_state(), BreakerState::Closed);
        assert!(!cb.is_open());
    }

    #[test]
    fn half_open_probe_fails_and_reopens() {
        let mut cfg = BreakerConfig::default();
        cfg.window_duration = Duration::from_secs(3600);
        cfg.min_samples = 1;
        cfg.open_duration = Duration::from_millis(1);
        let cb = CircuitBreaker::new(cfg);

        cb.record(Outcome::Failure);
        assert!(cb.is_open());

        std::thread::sleep(Duration::from_millis(10));
        assert!(cb.check().is_ok()); // probe admitted

        // Probe fails → back to Open.
        cb.record(Outcome::Failure);
        assert!(cb.is_open());
    }

    #[test]
    fn only_one_probe_at_a_time() {
        let mut cfg = BreakerConfig::default();
        cfg.window_duration = Duration::from_secs(3600);
        cfg.min_samples = 1;
        cfg.half_open_max_probes = 1;
        cfg.open_duration = Duration::from_millis(1);
        let cb = CircuitBreaker::new(cfg);

        cb.record(Outcome::Failure);
        std::thread::sleep(Duration::from_millis(10));

        assert!(cb.check().is_ok()); // first probe admitted
        assert!(cb.check().is_err()); // second rejected
    }

    #[test]
    fn failure_codes_detection() {
        let cfg = BreakerConfig::default();
        assert!(cfg.is_failure_status(500));
        assert!(cfg.is_failure_status(429));
        assert!(!cfg.is_failure_status(200));
        assert!(!cfg.is_failure_status(400));
    }

    #[test]
    fn sliding_window_error_rate() {
        let mut window = SlidingWindow::new();
        let now = Instant::now();

        window.push(now, true);
        window.push(now, false);
        window.push(now, true);
        assert_eq!(window.sample_count(), 3);
        assert!((window.error_rate() - 2.0 / 3.0).abs() < 0.01);

        window.clear();
        assert_eq!(window.sample_count(), 0);
        assert_eq!(window.error_rate(), 0.0);
    }
}
