//! Auth resilience — error taxonomy, circuit breaker, single-flight refresh.
//!
//! Design Doc 20 §9/§10:
//! - wire errors are normalized into an [`AuthErrorKind`] taxonomy so only
//!   401-class errors trigger refresh (403/policy NEVER refreshes);
//! - each account/audience carries an [`AuthBreakerState`] state machine
//!   (Healthy → Refreshing → Degraded → HalfOpen → Healthy, plus terminal
//!   ReauthRequired / Revoked) with a transient-failure cooldown and a
//!   single half-open probe;
//! - concurrent refresh demand for the same key is coalesced by
//!   [`SingleFlightRefresher`] so an N-way 401 storm triggers exactly ONE
//!   refresh call (acceptance #2: 100 concurrent 401s → 1 refresh).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{watch, Notify};

// ── Error taxonomy (Doc 20 §9) ─────────────────────────────────────

/// Normalized authentication error classes. The Provider Adapter maps wire
/// errors into this taxonomy; refresh decisions are made ONLY from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthErrorKind {
    /// Token close to expiry — refresh proactively.
    ExpiringSoon,
    /// 401 / token_invalid — eligible for a single-flight refresh.
    Unauthorized,
    /// 403 / policy / content restriction — NEVER refresh; surface as-is.
    PolicyDenied,
    /// Refresh token revoked/expired — terminal: ReauthRequired, open breaker.
    RefreshRevoked,
    /// Transient network / 5xx — bounded backoff, keep the unexpired token.
    Transient,
    /// Account mismatch — refuse to overwrite the current secret; re-login.
    AccountMismatch,
}

impl AuthErrorKind {
    /// Classify an HTTP status into the taxonomy (Doc 20 §9 table).
    /// 401 → refresh-eligible; 403 → policy (no refresh); 408/429/5xx →
    /// transient; everything else is treated as policy-denied (fail-closed:
    /// unknown statuses must never trigger a refresh).
    pub fn from_http_status(status: u16) -> Self {
        match status {
            401 => Self::Unauthorized,
            403 => Self::PolicyDenied,
            408 | 429 => Self::Transient,
            s if (500..600).contains(&s) => Self::Transient,
            _ => Self::PolicyDenied,
        }
    }

    /// Whether this error class permits a refresh attempt at all.
    pub fn refresh_eligible(&self) -> bool {
        matches!(self, Self::ExpiringSoon | Self::Unauthorized)
    }
}

/// The action a caller should take for a classified error (Doc 20 §9 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPolicy {
    /// Join/create the single-flight refresh; retry the request once on success.
    Refresh,
    /// Do not refresh — return the original error unchanged (403/policy).
    FailWithoutRefresh,
    /// Refresh token is dead — mark ReauthRequired; the breaker stops loops.
    ReauthRequired,
    /// Transient failure — bounded backoff, keep using the unexpired token.
    BackoffKeepToken,
    /// Refuse to overwrite the current secret; the user must re-login.
    RequireRelogin,
}

impl RefreshPolicy {
    /// Map a classified error to the caller action (Doc 20 §9 table).
    pub fn for_error(kind: AuthErrorKind) -> Self {
        match kind {
            AuthErrorKind::ExpiringSoon | AuthErrorKind::Unauthorized => Self::Refresh,
            AuthErrorKind::PolicyDenied => Self::FailWithoutRefresh,
            AuthErrorKind::RefreshRevoked => Self::ReauthRequired,
            AuthErrorKind::Transient => Self::BackoffKeepToken,
            AuthErrorKind::AccountMismatch => Self::RequireRelogin,
        }
    }
}

// ── Circuit breaker (Doc 20 §10) ───────────────────────────────────

/// Breaker state for one account/audience key (Doc 20 §10 state diagram).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthBreakerState {
    /// Normal operation.
    Healthy,
    /// A refresh is in flight (informational — requests may still join it).
    Refreshing,
    /// Transient failures tripped the breaker; cooling down.
    Degraded,
    /// Cooldown elapsed; exactly ONE probe refresh is allowed.
    HalfOpen,
    /// Permanent refresh failure — no further refresh attempts, ever,
    /// until the user re-authenticates (acceptance #4: no loops).
    ReauthRequired,
    /// Credentials explicitly revoked (logout).
    Revoked,
}

/// One breaker entry. `allow_refresh` is the single gate every refresh
/// attempt must pass through.
#[derive(Debug)]
struct BreakerEntry {
    state: AuthBreakerState,
    /// Consecutive transient failures since the last success.
    transient_failures: u32,
    /// When `Degraded`, the instant at which a half-open probe is allowed.
    cooldown_until: Option<Instant>,
    /// Whether the single half-open probe is already in flight.
    probe_in_flight: bool,
}

impl BreakerEntry {
    fn new() -> Self {
        Self {
            state: AuthBreakerState::Healthy,
            transient_failures: 0,
            cooldown_until: None,
            probe_in_flight: false,
        }
    }
}

/// Per-(account, audience) circuit breaker registry.
///
/// All state transitions enforce Doc 20 §10: permanent failures are terminal,
/// degraded keys cool down before a single half-open probe, and success
/// always restores Healthy.
pub struct AuthCircuitBreaker {
    entries: Mutex<HashMap<String, BreakerEntry>>,
    /// Number of consecutive transient failures that trip Degraded.
    trip_threshold: u32,
    /// Cooldown before a half-open probe is allowed.
    cooldown: Duration,
}

impl AuthCircuitBreaker {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            trip_threshold: 3,
            cooldown: Duration::from_secs(30),
        }
    }

    /// Test/override constructor.
    pub fn with_params(trip_threshold: u32, cooldown: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            trip_threshold: trip_threshold.max(1),
            cooldown,
        }
    }

    /// The single gate: may a refresh attempt for `key` right now?
    /// Half-open admits exactly one probe; Degraded admits nothing until
    /// its cooldown elapses (then it behaves as half-open); terminal
    /// states admit nothing, ever.
    pub fn allow_refresh(&self, key: &str) -> bool {
        let mut entries = self.entries.lock().expect("breaker mutex poisoned");
        let entry = entries.entry(key.to_string()).or_insert_with(BreakerEntry::new);
        match entry.state {
            AuthBreakerState::Healthy | AuthBreakerState::Refreshing => true,
            AuthBreakerState::ReauthRequired | AuthBreakerState::Revoked => false,
            AuthBreakerState::Degraded => {
                let ready = entry
                    .cooldown_until
                    .is_some_and(|t| Instant::now() >= t);
                if ready && !entry.probe_in_flight {
                    // Promote to half-open and claim the single probe slot.
                    entry.state = AuthBreakerState::HalfOpen;
                    entry.probe_in_flight = true;
                    true
                } else {
                    false
                }
            }
            AuthBreakerState::HalfOpen => {
                // Only ONE probe at a time (Doc 20 §10).
                if entry.probe_in_flight {
                    false
                } else {
                    entry.probe_in_flight = true;
                    true
                }
            }
        }
    }

    /// Mark a refresh attempt starting (Healthy → Refreshing).
    pub fn mark_refreshing(&self, key: &str) {
        let mut entries = self.entries.lock().expect("breaker mutex poisoned");
        let entry = entries.entry(key.to_string()).or_insert_with(BreakerEntry::new);
        if matches!(
            entry.state,
            AuthBreakerState::Healthy | AuthBreakerState::HalfOpen
        ) {
            if entry.state == AuthBreakerState::Healthy {
                entry.state = AuthBreakerState::Refreshing;
            }
        }
    }

    /// Record a successful refresh — always restores Healthy and clears
    /// failure counters / probe bookkeeping.
    pub fn record_success(&self, key: &str) {
        let mut entries = self.entries.lock().expect("breaker mutex poisoned");
        let entry = entries.entry(key.to_string()).or_insert_with(BreakerEntry::new);
        entry.state = AuthBreakerState::Healthy;
        entry.transient_failures = 0;
        entry.cooldown_until = None;
        entry.probe_in_flight = false;
    }

    /// Record a refresh failure classified as `kind` (Doc 20 §9/§10 rules):
    /// - permanent (revoked) → terminal ReauthRequired, no loops;
    /// - transient → bounded: after `trip_threshold` consecutive failures
    ///   enter Degraded with a cooldown;
    /// - policy/account errors are NOT breaker events (they never refresh).
    pub fn record_failure(&self, key: &str, kind: AuthErrorKind) {
        let mut entries = self.entries.lock().expect("breaker mutex poisoned");
        let entry = entries.entry(key.to_string()).or_insert_with(BreakerEntry::new);
        match kind {
            AuthErrorKind::RefreshRevoked | AuthErrorKind::AccountMismatch => {
                entry.state = AuthBreakerState::ReauthRequired;
                entry.probe_in_flight = false;
            }
            AuthErrorKind::Transient => {
                entry.transient_failures = entry.transient_failures.saturating_add(1);
                entry.probe_in_flight = false;
                if entry.transient_failures >= self.trip_threshold {
                    entry.state = AuthBreakerState::Degraded;
                    entry.cooldown_until = Some(Instant::now() + self.cooldown);
                } else if entry.state == AuthBreakerState::HalfOpen {
                    // Failed probe: back to Degraded with a fresh cooldown.
                    entry.state = AuthBreakerState::Degraded;
                    entry.cooldown_until = Some(Instant::now() + self.cooldown);
                }
            }
            // Unauthorized/ExpiringSoon failures during a refresh are handled
            // by the caller's retry policy, not the breaker; PolicyDenied
            // should never reach a refresh attempt in the first place.
            AuthErrorKind::Unauthorized | AuthErrorKind::ExpiringSoon => {
                entry.probe_in_flight = false;
                if entry.state == AuthBreakerState::Refreshing {
                    entry.state = AuthBreakerState::Healthy;
                }
            }
            AuthErrorKind::PolicyDenied => {
                entry.probe_in_flight = false;
            }
        }
    }

    /// Explicitly revoke a key (logout). Old leases become unusable via the
    /// broker's epoch bump; the breaker records the terminal state.
    pub fn revoke(&self, key: &str) {
        let mut entries = self.entries.lock().expect("breaker mutex poisoned");
        let entry = entries.entry(key.to_string()).or_insert_with(BreakerEntry::new);
        entry.state = AuthBreakerState::Revoked;
    }

    /// Re-authentication performed by the user: clears a terminal state so
    /// the key becomes usable again.
    pub fn reset_after_reauth(&self, key: &str) {
        let mut entries = self.entries.lock().expect("breaker mutex poisoned");
        entries.insert(key.to_string(), BreakerEntry::new());
    }

    /// Current state for a key (Healthy when unknown).
    pub fn state(&self, key: &str) -> AuthBreakerState {
        self.entries
            .lock()
            .expect("breaker mutex poisoned")
            .get(key)
            .map(|e| e.state)
            .unwrap_or(AuthBreakerState::Healthy)
    }
}

impl Default for AuthCircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

// ── Single-flight refresh (Doc 20 §9) ──────────────────────────────

/// Boxed async refresh closure. Returns the fresh credential material on
/// success or the classified error kind on failure.
pub type RefreshFn<T> = Box<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<T, AuthErrorKind>> + Send>> + Send + Sync,
>;

struct Flight<T> {
    /// Woken exactly once when the leader's refresh completes.
    done: Arc<Notify>,
    /// Result broadcast: `None` until the leader finishes.
    result: watch::Sender<Option<Result<T, AuthErrorKind>>>,
}

/// Coalesces concurrent refresh demand per key: the FIRST caller runs the
/// refresh closure; every other concurrent caller waits and receives the
/// SAME result. Acceptance #2: 100 concurrent 401s → exactly one refresh.
///
/// Flights are per-key and single-use: after completion the entry is
/// evicted, so a LATER 401 storm starts a fresh flight (bounded retries
/// remain the caller's responsibility — "最多重试一次").
pub struct SingleFlightRefresher<T: Clone> {
    flights: Mutex<HashMap<String, Arc<Flight<T>>>>,
}

impl<T: Clone> SingleFlightRefresher<T> {
    pub fn new() -> Self {
        Self {
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// Number of refreshes currently in flight (for tests/telemetry).
    pub fn in_flight(&self) -> usize {
        self.flights.lock().expect("single-flight mutex poisoned").len()
    }
}

impl<T: Clone + Send + Sync + 'static> SingleFlightRefresher<T> {
    /// Join or create the refresh flight for `key`.
    pub async fn refresh_or_join(&self, key: &str, f: &RefreshFn<T>) -> Result<T, AuthErrorKind> {
        // Phase 1 (lock, no await): join an existing flight, or claim the
        // leader slot by inserting our flight if the key is vacant.
        let flight = Arc::new(Flight {
            done: Arc::new(Notify::new()),
            result: watch::channel(None).0,
        });
        let existing = {
            let mut flights = self.flights.lock().expect("single-flight mutex poisoned");
            match flights.entry(key.to_string()) {
                std::collections::hash_map::Entry::Occupied(o) => Some(o.get().clone()),
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(flight.clone());
                    None
                }
            }
        };

        // Phase 2 (no lock): either wait on the leader's result, or run the
        // refresh ourselves and publish.
        if let Some(existing) = existing {
            return Self::await_flight(&existing).await;
        }

        let outcome = f().await;
        // Publish result BEFORE waking so every waiter observes it.
        let _ = flight.result.send(Some(outcome.clone()));
        flight.done.notify_waiters();
        // Evict the finished flight so later storms get a fresh one.
        self.flights
            .lock()
            .expect("single-flight mutex poisoned")
            .remove(key);
        outcome
    }

    async fn await_flight(flight: &Arc<Flight<T>>) -> Result<T, AuthErrorKind> {
        // Already finished? Read without waiting.
        if let Some(res) = flight.result.borrow().as_ref() {
            return res.clone();
        }
        let mut rx = flight.result.subscribe();
        loop {
            // Wait for either the result or the completion notification.
            tokio::select! {
                _ = flight.done.notified() => {}
                changed = rx.changed() => {
                    if changed.is_err() {
                        // Sender dropped without a result — treat as transient.
                        return Err(AuthErrorKind::Transient);
                    }
                }
            }
            if let Some(res) = flight.result.borrow().as_ref() {
                return res.clone();
            }
        }
    }
}

impl<T: Clone> Default for SingleFlightRefresher<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn taxonomy_http_status_mapping() {
        assert_eq!(AuthErrorKind::from_http_status(401), AuthErrorKind::Unauthorized);
        assert_eq!(AuthErrorKind::from_http_status(403), AuthErrorKind::PolicyDenied);
        assert_eq!(AuthErrorKind::from_http_status(429), AuthErrorKind::Transient);
        assert_eq!(AuthErrorKind::from_http_status(503), AuthErrorKind::Transient);
        // Unknown / policy statuses never refresh (fail-closed).
        assert_eq!(AuthErrorKind::from_http_status(418), AuthErrorKind::PolicyDenied);
    }

    #[test]
    fn policy_rules_doc20_section9() {
        // Acceptance #3: 403 does not refresh.
        assert_eq!(
            RefreshPolicy::for_error(AuthErrorKind::PolicyDenied),
            RefreshPolicy::FailWithoutRefresh
        );
        assert_eq!(
            RefreshPolicy::for_error(AuthErrorKind::Unauthorized),
            RefreshPolicy::Refresh
        );
        assert_eq!(
            RefreshPolicy::for_error(AuthErrorKind::RefreshRevoked),
            RefreshPolicy::ReauthRequired
        );
        assert_eq!(
            RefreshPolicy::for_error(AuthErrorKind::Transient),
            RefreshPolicy::BackoffKeepToken
        );
        assert!(!AuthErrorKind::PolicyDenied.refresh_eligible());
        assert!(AuthErrorKind::Unauthorized.refresh_eligible());
    }

    #[test]
    fn breaker_permanent_failure_terminal_no_loop() {
        // Acceptance #4: permanent refresh failure → ReauthRequired, no loops.
        let br = AuthCircuitBreaker::new();
        assert!(br.allow_refresh("k"));
        br.record_failure("k", AuthErrorKind::RefreshRevoked);
        assert_eq!(br.state("k"), AuthBreakerState::ReauthRequired);
        // No amount of time or retries re-opens a terminal state.
        assert!(!br.allow_refresh("k"));
        assert!(!br.allow_refresh("k"));
        // Only an explicit user re-auth clears it.
        br.reset_after_reauth("k");
        assert!(br.allow_refresh("k"));
        assert_eq!(br.state("k"), AuthBreakerState::Healthy);
    }

    #[test]
    fn breaker_transient_trip_and_halfopen_single_probe() {
        let br = AuthCircuitBreaker::with_params(2, Duration::ZERO);
        br.record_failure("k", AuthErrorKind::Transient);
        assert!(br.allow_refresh("k"), "below threshold still healthy");
        br.record_failure("k", AuthErrorKind::Transient);
        assert_eq!(br.state("k"), AuthBreakerState::Degraded);
        // Cooldown=0 → immediately eligible, but exactly ONE probe.
        assert!(br.allow_refresh("k"));
        assert!(!br.allow_refresh("k"), "second concurrent probe must be denied");
        // Successful probe restores Healthy.
        br.record_success("k");
        assert_eq!(br.state("k"), AuthBreakerState::Healthy);
        assert!(br.allow_refresh("k"));
    }

    #[test]
    fn breaker_failed_probe_returns_to_degraded() {
        let br = AuthCircuitBreaker::with_params(1, Duration::ZERO);
        br.record_failure("k", AuthErrorKind::Transient);
        assert!(br.allow_refresh("k")); // claim the probe
        br.record_failure("k", AuthErrorKind::Transient); // probe failed
        assert_eq!(br.state("k"), AuthBreakerState::Degraded);
    }

    #[tokio::test]
    async fn single_flight_100_concurrent_401s_one_refresh() {
        // Acceptance #2: 100 concurrent 401 → exactly one refresh call.
        let sf = Arc::new(SingleFlightRefresher::<String>::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..100 {
            let sf = sf.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                let calls = calls.clone();
                let f: RefreshFn<String> = Box::new(move || {
                    let calls = calls.clone();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // Small delay so the other 99 callers pile in.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok("fresh-token".to_string())
                    })
                });
                sf.refresh_or_join("openai", &f).await
            }));
        }

        let mut ok = 0;
        for h in handles {
            let res = h.await.unwrap();
            assert_eq!(res.unwrap(), "fresh-token");
            ok += 1;
        }
        assert_eq!(ok, 100, "all callers must receive the refreshed token");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "refresh must run exactly once");
        assert_eq!(sf.in_flight(), 0, "flight must be evicted after completion");
    }

    #[tokio::test]
    async fn single_flight_error_shared_and_later_storm_refreshes_again() {
        let sf = Arc::new(SingleFlightRefresher::<String>::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let make_f = |calls: Arc<AtomicUsize>, fail: bool| {
            let f: RefreshFn<String> = Box::new(move || {
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    // Give the other joiners time to pile into this flight.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if fail {
                        Err(AuthErrorKind::RefreshRevoked)
                    } else {
                        Ok("ok".to_string())
                    }
                })
            });
            f
        };

        // Storm 1: failure is shared by all joiners.
        let mut h = Vec::new();
        for _ in 0..5 {
            let sf = sf.clone();
            let f = make_f(calls.clone(), true);
            h.push(tokio::spawn(async move { sf.refresh_or_join("k", &f).await }));
        }
        for t in h {
            assert_eq!(t.await.unwrap().unwrap_err(), AuthErrorKind::RefreshRevoked);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Storm 2 (later): a fresh flight runs again (bounded retry is the
        // caller's job; the refresher must not cache the dead flight).
        let f = make_f(calls.clone(), false);
        assert_eq!(sf.refresh_or_join("k", &f).await.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn single_flight_distinct_keys_run_independently() {
        let sf = Arc::new(SingleFlightRefresher::<String>::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut h = Vec::new();
        for key in ["a", "b", "c"] {
            let sf = sf.clone();
            let calls = calls.clone();
            let key_owned = key.to_string();
            let key_for_call = key.to_string();
            h.push(tokio::spawn(async move {
                let calls = calls.clone();
                let result_key = key_owned.clone();
                let f: RefreshFn<String> = Box::new(move || {
                    let calls = calls.clone();
                    let result_key = result_key.clone();
                    Box::pin(async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        Ok(result_key)
                    })
                });
                sf.refresh_or_join(&key_for_call, &f).await
            }));
        }
        for t in h {
            assert!(t.await.unwrap().is_ok());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
