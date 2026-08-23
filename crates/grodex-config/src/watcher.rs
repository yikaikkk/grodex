//! ConfigWatcher — event-driven config publish pipeline with breaker & LKG.
//!
//! Design Doc 18 §11/§12 and acceptance #2/#3/#4/#5/#8:
//!
//! - **#8**: editors emit bursts of fs events for one logical save
//!   (atomic-save = write-temp + rename, plus duplicate events). All of
//!   them carry IDENTICAL content, so content-hash dedup collapses the
//!   burst to a single publish;
//! - **#2**: a malformed candidate never replaces a valid generation —
//!   the last-known-good state is preserved and diagnostics surface;
//! - **#3**: the breaker key is `source_id + content_hash + domain`;
//!   a known-bad hash short-circuits straight to the cached diagnostic
//!   without re-compiling, while a changed hash immediately re-arms;
//! - **#4**: Closed→Open after `failure_threshold` failures, exponential
//!   cooldown, content change transitions to HalfOpen for a single full
//!   validation probe — success publishes and closes the breaker;
//! - **#5**: each domain carries independent state: a broken UI theme
//!   must not block a Policy tightening;
//! - **§12**: every domain records its Last-Known-Good publish (hash +
//!   generation) for degradation decisions.
//!
//! The watcher is fs-backend agnostic: whatever watches the files
//! (`notify`, polling, remote sync) calls [`ConfigWatcher::observe`]
//! with the new content; the pipeline owns everything from dedup to
//! publish accounting.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Failure domains that breaker/LKG state is partitioned by (Doc 18 §12).
/// A bad candidate in one domain must never block publishes in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigDomain {
    /// Whole-config root publish (the default coarse granularity).
    Root,
    Ui,
    Memory,
    Mcp,
    SkillHook,
    ModelRoute,
    Prompt,
    Policy,
    Managed,
    Sandbox,
    Credential,
}

/// Breaker state machine (Doc 18 §11.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    /// Rejecting; `until` is when the cooldown expires, `cooldown` the
    /// current backoff (doubles on every failed HalfOpen probe).
    Open { until: Instant, cooldown: Duration },
    /// A single full-validation probe is in flight / permitted.
    HalfOpen,
}

/// Tunables for the publish breaker. Defaults are operational values
/// per §11.3 ("阈值、窗口和 cooldown 是运维参数，不写死在业务逻辑").
#[derive(Debug, Clone)]
pub struct PublishBreakerConfig {
    /// Consecutive failures before Closed → Open.
    pub failure_threshold: u32,
    /// Initial cooldown once Open.
    pub base_cooldown: Duration,
    /// Cooldown ceiling for the exponential backoff.
    pub max_cooldown: Duration,
}

impl Default for PublishBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            base_cooldown: Duration::from_secs(5),
            max_cooldown: Duration::from_secs(300),
        }
    }
}

/// Per-key publish breaker: Closed/Open/HalfOpen with exponential
/// cooldown. Content-hash change always re-arms (HalfOpen probe).
#[derive(Debug)]
pub struct PublishBreaker {
    config: PublishBreakerConfig,
    state: BreakerState,
    consecutive_failures: u32,
    /// Cooldown applied on the last trip — needed to double it when a
    /// HalfOpen probe fails (§11.3 exponential backoff).
    last_cooldown: Duration,
}

impl PublishBreaker {
    pub fn new(config: PublishBreakerConfig) -> Self {
        Self {
            config,
            state: BreakerState::Closed,
            consecutive_failures: 0,
            last_cooldown: Duration::ZERO,
        }
    }

    pub fn state(&self) -> &BreakerState {
        &self.state
    }

    /// May this (possibly new) content attempt a compile right now?
    /// Transitions Open→HalfOpen when the cooldown expired OR the content
    /// hash differs from the one that tripped the breaker (§11.2/#4).
    pub fn admit(&mut self, content_hash: &str, last_failed_hash: Option<&str>, now: Instant) -> bool {
        match &self.state {
            BreakerState::Closed | BreakerState::HalfOpen => true,
            BreakerState::Open { until, .. } => {
                let content_changed =
                    last_failed_hash.map(|h| h != content_hash).unwrap_or(true);
                if content_changed || now >= *until {
                    self.state = BreakerState::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Record a successful publish: reset to Closed.
    pub fn record_success(&mut self) {
        self.state = BreakerState::Closed;
        self.consecutive_failures = 0;
    }

    /// Record a failed attempt; trips Open when the threshold is reached
    /// (or immediately from HalfOpen), doubling the cooldown each trip.
    pub fn record_failure(&mut self, now: Instant) {
        self.consecutive_failures += 1;
        let trip = match &self.state {
            BreakerState::HalfOpen => true,
            _ => self.consecutive_failures >= self.config.failure_threshold,
        };
        if trip {
            // HalfOpen probe failed → double the previous cooldown;
            // fresh trip from Closed starts at the base cooldown.
            let next = if matches!(self.state, BreakerState::HalfOpen) {
                (self.last_cooldown * 2).min(self.config.max_cooldown)
            } else {
                self.config.base_cooldown
            };
            self.last_cooldown = next;
            self.state = BreakerState::Open {
                until: now + next,
                cooldown: next,
            };
        } else {
            self.state = BreakerState::Closed;
        }
    }
}

/// Last-Known-Good record for one domain (Doc 18 §12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastKnownGood {
    pub content_hash: String,
    pub generation: u64,
}

/// Outcome of one [`ConfigWatcher::observe`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchOutcome {
    /// Content identical to the last successful publish — deduplicated,
    /// no rebuild (acceptance #8: duplicate/atomic-save event bursts).
    Unchanged,
    /// Candidate validated and published with a new generation.
    Published { generation: u64 },
    /// Candidate rejected by validation; last-known-good retained
    /// (acceptance #2). Diagnostics are surfaced, never swallowed.
    Rejected { diagnostic: String },
    /// Same bad hash seen before — cached diagnostic reused, compile was
    /// NOT re-run (acceptance #3).
    CachedFailure { diagnostic: String },
    /// Breaker Open and cooldown not expired for this unchanged content
    /// (acceptance #4: retry after cooldown or on content change).
    BreakerOpen { retry_after: Duration },
}

/// Per-domain publish state: breaker + last hashes + LKG.
#[derive(Debug)]
struct DomainState {
    breaker: PublishBreaker,
    /// Hash of the last SUCCESSFULLY published content.
    published_hash: Option<String>,
    /// Hash of the most recent failed compile attempt.
    last_failed_hash: Option<String>,
    /// Cached diagnostic for `last_failed_hash` (acceptance #3).
    cached_failure: Option<String>,
    /// Last-Known-Good publish record (§12).
    lkg: Option<LastKnownGood>,
    /// Current live generation for this domain.
    generation: u64,
    /// How many times `validate` was actually invoked (test observability).
    compile_attempts: u64,
}

/// The watcher pipeline. One instance per watched source set; state is
/// partitioned by [`ConfigDomain`] so failures stay isolated (#5).
#[derive(Debug)]
pub struct ConfigWatcher {
    domains: HashMap<ConfigDomain, DomainState>,
    breaker_config: PublishBreakerConfig,
    /// Total successful publishes across all domains (audit counter).
    total_publishes: u64,
}

impl Default for ConfigWatcher {
    fn default() -> Self {
        Self::with_breaker_config(PublishBreakerConfig::default())
    }
}

impl ConfigWatcher {
    pub fn with_breaker_config(breaker_config: PublishBreakerConfig) -> Self {
        Self {
            domains: HashMap::new(),
            breaker_config,
            total_publishes: 0,
        }
    }

    /// SHA-256 of candidate content (the dedup/breaker identity).
    pub fn content_hash(content: &[u8]) -> String {
        format!("{:x}", Sha256::digest(content))
    }

    fn domain(&mut self, domain: ConfigDomain) -> &mut DomainState {
        // Copy the tunables first: `entry()` borrows `self.domains`
        // mutably, so `self.breaker_config` must be read beforehand.
        let cfg = self.breaker_config.clone();
        self.domains.entry(domain).or_insert_with(|| DomainState {
            breaker: PublishBreaker::new(cfg),
            published_hash: None,
            last_failed_hash: None,
            cached_failure: None,
            lkg: None,
            generation: 0,
            compile_attempts: 0,
        })
    }

    /// Observe new candidate content for `domain` from `source`.
    ///
    /// `validate` runs the FULL candidate validation pipeline (parse →
    /// validate → compile, Doc 18 §10) and returns the generation to
    /// publish on success. It is only invoked when the pipeline decides
    /// a rebuild is warranted — the closure's call count is exactly the
    /// number of real compiles performed.
    pub fn observe(
        &mut self,
        domain: ConfigDomain,
        _source: &str,
        content: &[u8],
        now: Instant,
        validate: impl FnOnce(&[u8]) -> Result<u64, String>,
    ) -> WatchOutcome {
        let hash = Self::content_hash(content);

        // #8: identical to the live publish → dedup, no rebuild.
        {
            let st = self.domain(domain);
            if st.published_hash.as_deref() == Some(hash.as_str()) {
                return WatchOutcome::Unchanged;
            }
            // #3: known-bad hash → cached diagnostic, no re-compile.
            // Exception: when the breaker admits a HalfOpen probe
            // (cooldown expired), the probe must run a FULL validation
            // (§11.3) instead of returning the cache.
            if st.last_failed_hash.as_deref() == Some(hash.as_str()) {
                if !st.breaker.admit(&hash, st.last_failed_hash.as_deref(), now) {
                    if let BreakerState::Open { until, .. } = st.breaker.state().clone() {
                        return WatchOutcome::BreakerOpen {
                            retry_after: until.saturating_duration_since(now),
                        };
                    }
                }
                if st.breaker.state() != &BreakerState::HalfOpen {
                    let diagnostic = st
                        .cached_failure
                        .clone()
                        .unwrap_or_else(|| "cached failure (diagnostic lost)".into());
                    return WatchOutcome::CachedFailure { diagnostic };
                }
            }
        }

        // Breaker gate for (possibly new) content.
        {
            let st = self.domain(domain);
            if !st.breaker.admit(&hash, st.last_failed_hash.as_deref(), now) {
                if let BreakerState::Open { until, .. } = st.breaker.state().clone() {
                    return WatchOutcome::BreakerOpen {
                        retry_after: until.saturating_duration_since(now),
                    };
                }
            }
        }

        // Full validation — the only place a real compile happens.
        let st = self.domain(domain);
        st.compile_attempts += 1;
        match validate(content) {
            Ok(generation) => {
                st.generation = generation;
                st.published_hash = Some(hash.clone());
                st.last_failed_hash = None;
                st.cached_failure = None;
                st.lkg = Some(LastKnownGood {
                    content_hash: hash,
                    generation,
                });
                st.breaker.record_success();
                self.total_publishes += 1;
                WatchOutcome::Published { generation }
            }
            Err(diagnostic) => {
                // #2: live generation untouched — LKG stays valid.
                st.last_failed_hash = Some(hash);
                st.cached_failure = Some(diagnostic.clone());
                st.breaker.record_failure(now);
                WatchOutcome::Rejected { diagnostic }
            }
        }
    }

    /// Manual half-open probe (`config validate --force`, §11.3): forces
    /// one full validation for `domain` regardless of breaker state, but
    /// NEVER skips validation itself.
    pub fn force_probe(
        &mut self,
        domain: ConfigDomain,
        source: &str,
        content: &[u8],
        now: Instant,
        validate: impl FnOnce(&[u8]) -> Result<u64, String>,
    ) -> WatchOutcome {
        let cfg = self.breaker_config.clone();
        let base = cfg.base_cooldown;
        let st = self.domain(domain);
        st.breaker = PublishBreaker::new(cfg);
        st.breaker.state = BreakerState::HalfOpen;
        // A failed manual probe should still back off meaningfully.
        st.breaker.last_cooldown = base;
        // Clear the bad-hash cache so the probe actually compiles.
        st.last_failed_hash = None;
        st.cached_failure = None;
        self.observe(domain, source, content, now, validate)
    }

    /// Last-Known-Good record for a domain (§12 degradation decisions).
    pub fn last_known_good(&self, domain: ConfigDomain) -> Option<&LastKnownGood> {
        self.domains.get(&domain).and_then(|s| s.lkg.as_ref())
    }

    /// Current live generation for a domain.
    pub fn generation(&self, domain: ConfigDomain) -> u64 {
        self.domains.get(&domain).map(|s| s.generation).unwrap_or(0)
    }

    /// Current breaker state for a domain.
    pub fn breaker_state(&self, domain: ConfigDomain) -> Option<&BreakerState> {
        self.domains.get(&domain).map(|s| s.breaker.state())
    }

    /// How many real validations were performed for a domain (auditing
    /// for acceptance #3/#8 — bursts must not multiply compiles).
    pub fn compile_attempts(&self, domain: ConfigDomain) -> u64 {
        self.domains.get(&domain).map(|s| s.compile_attempts).unwrap_or(0)
    }

    /// Total successful publishes across all domains.
    pub fn total_publishes(&self) -> u64 {
        self.total_publishes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_validate(generation: u64) -> impl FnOnce(&[u8]) -> Result<u64, String> {
        move |_| Ok(generation)
    }

    fn fail_validate(msg: &'static str) -> impl FnOnce(&[u8]) -> Result<u64, String> {
        move |_| Err(msg.into())
    }

    #[test]
    fn duplicate_events_publish_once() {
        // Acceptance #8: a burst of identical events (duplicate fs
        // notifications) collapses to a single publish.
        let mut w = ConfigWatcher::default();
        let now = Instant::now();
        let content = b"[ui]\ntheme = \"dark\"";

        let r1 = w.observe(ConfigDomain::Ui, "workspace", content, now, ok_validate(1));
        assert_eq!(r1, WatchOutcome::Published { generation: 1 });
        for _ in 0..5 {
            let r = w.observe(ConfigDomain::Ui, "workspace", content, now, ok_validate(2));
            assert_eq!(r, WatchOutcome::Unchanged);
        }
        assert_eq!(w.total_publishes(), 1, "burst must publish exactly once");
        assert_eq!(w.compile_attempts(ConfigDomain::Ui), 1);
    }

    #[test]
    fn atomic_save_two_events_one_publish() {
        // Acceptance #8: atomic save = write-temp + rename → two events
        // with identical content → one publish.
        let mut w = ConfigWatcher::default();
        let now = Instant::now();
        let content = b"model_id = \"gpt-5\"";

        let r1 = w.observe(ConfigDomain::Root, "user", content, now, ok_validate(1)); // write temp
        assert_eq!(r1, WatchOutcome::Published { generation: 1 });
        let r2 = w.observe(ConfigDomain::Root, "user", content, now, ok_validate(2)); // rename
        assert_eq!(r2, WatchOutcome::Unchanged);
        assert_eq!(w.total_publishes(), 1);
    }

    #[test]
    fn malformed_content_never_replaces_valid_generation() {
        // Acceptance #2: rejection keeps the live generation + LKG intact.
        let mut w = ConfigWatcher::default();
        let now = Instant::now();

        let r1 = w.observe(ConfigDomain::Root, "user", b"good = 1", now, ok_validate(7));
        assert_eq!(r1, WatchOutcome::Published { generation: 7 });

        let r2 = w.observe(
            ConfigDomain::Root,
            "user",
            b"[[[broken",
            now,
            fail_validate("toml parse error"),
        );
        assert!(matches!(r2, WatchOutcome::Rejected { .. }));
        assert_eq!(w.generation(ConfigDomain::Root), 7, "live generation preserved");
        let lkg = w.last_known_good(ConfigDomain::Root).unwrap();
        assert_eq!(lkg.generation, 7);
    }

    #[test]
    fn same_bad_hash_reuses_cached_diagnostic_without_recompile() {
        // Acceptance #3: known-bad hash short-circuits; compile count
        // stays at 1 no matter how many events arrive.
        let cfg = PublishBreakerConfig {
            failure_threshold: 10, // stay Closed to isolate the hash cache
            ..Default::default()
        };
        let mut w = ConfigWatcher::with_breaker_config(cfg);
        let now = Instant::now();
        let bad = b"[[[broken";

        let r1 = w.observe(ConfigDomain::Ui, "user", bad, now, fail_validate("boom"));
        assert!(matches!(r1, WatchOutcome::Rejected { .. }));
        for _ in 0..4 {
            let r = w.observe(ConfigDomain::Ui, "user", bad, now, fail_validate("boom"));
            assert_eq!(
                r,
                WatchOutcome::CachedFailure {
                    diagnostic: "boom".into()
                }
            );
        }
        assert_eq!(w.compile_attempts(ConfigDomain::Ui), 1, "no re-compile for same bad hash");
    }

    #[test]
    fn breaker_opens_and_recovers_via_halfopen_on_content_change() {
        // Acceptance #4: threshold → Open; changed content → HalfOpen
        // probe; success → Closed + publish.
        let cfg = PublishBreakerConfig {
            failure_threshold: 2,
            base_cooldown: Duration::from_secs(600), // long: force content-driven recovery
            ..Default::default()
        };
        let mut w = ConfigWatcher::with_breaker_config(cfg);
        let now = Instant::now();

        for i in 0..2 {
            let r = w.observe(
                ConfigDomain::Mcp,
                "user",
                format!("bad-{i}").as_bytes(),
                now,
                fail_validate("invalid"),
            );
            assert!(matches!(r, WatchOutcome::Rejected { .. }));
        }
        assert_eq!(
            w.breaker_state(ConfigDomain::Mcp),
            Some(&BreakerState::Open {
                until: now + Duration::from_secs(600),
                cooldown: Duration::from_secs(600),
            })
        );

        // Same-bad-hash event while Open → BreakerOpen (gated).
        let r = w.observe(ConfigDomain::Mcp, "user", b"bad-1", now, fail_validate("x"));
        assert!(matches!(r, WatchOutcome::BreakerOpen { .. }));

        // Fixed content re-arms (HalfOpen) even during cooldown, and the
        // successful probe publishes + closes the breaker.
        let r = w.observe(ConfigDomain::Mcp, "user", b"fixed = true", now, ok_validate(3));
        assert_eq!(r, WatchOutcome::Published { generation: 3 });
        assert_eq!(w.breaker_state(ConfigDomain::Mcp), Some(&BreakerState::Closed));
    }

    #[test]
    fn halfopen_failure_doubles_cooldown() {
        // §11.3: HalfOpen probe failure → Open with exponential cooldown.
        let cfg = PublishBreakerConfig {
            failure_threshold: 1,
            base_cooldown: Duration::from_secs(10),
            max_cooldown: Duration::from_secs(300),
        };
        let mut w = ConfigWatcher::with_breaker_config(cfg.clone());
        let now = Instant::now();

        let _ = w.observe(ConfigDomain::Root, "user", b"bad1", now, fail_validate("e"));
        // Tripped: cooldown 10s. New content at t+1 → HalfOpen probe;
        // failure → cooldown doubles to 20s (measured from the probe time).
        let probe_at = now + Duration::from_secs(1);
        let _ = w.observe(ConfigDomain::Root, "user", b"bad2", probe_at, fail_validate("e"));
        assert_eq!(
            w.breaker_state(ConfigDomain::Root),
            Some(&BreakerState::Open {
                until: probe_at + Duration::from_secs(20),
                cooldown: Duration::from_secs(20),
            }),
            "failed HalfOpen probe must double the cooldown"
        );
    }

    #[test]
    fn cooldown_expiry_rearms_halfopen() {
        // §11.3: Open + cooldown expired → HalfOpen probe allowed.
        let cfg = PublishBreakerConfig {
            failure_threshold: 1,
            base_cooldown: Duration::from_secs(5),
            max_cooldown: Duration::from_secs(300),
        };
        let mut w = ConfigWatcher::with_breaker_config(cfg);
        let now = Instant::now();

        let _ = w.observe(ConfigDomain::Root, "user", b"same-bad", now, fail_validate("e"));
        assert!(matches!(
            w.breaker_state(ConfigDomain::Root),
            Some(BreakerState::Open { .. })
        ));
        // Before expiry: same content is gated.
        let r = w.observe(ConfigDomain::Root, "user", b"same-bad", now + Duration::from_secs(1), fail_validate("e"));
        assert!(matches!(r, WatchOutcome::BreakerOpen { .. }));
        // After expiry: probe allowed (same hash ok — time-based re-arm).
        let r = w.observe(
            ConfigDomain::Root,
            "user",
            b"same-bad",
            now + Duration::from_secs(6),
            ok_validate(9),
        );
        assert_eq!(r, WatchOutcome::Published { generation: 9 });
    }

    #[test]
    fn domain_failures_are_isolated() {
        // Acceptance #5: UI breaker Open must not block Policy publishes.
        let cfg = PublishBreakerConfig {
            failure_threshold: 1,
            ..Default::default()
        };
        let mut w = ConfigWatcher::with_breaker_config(cfg);
        let now = Instant::now();

        let _ = w.observe(ConfigDomain::Ui, "user", b"bad-theme", now, fail_validate("theme broken"));
        assert!(matches!(
            w.breaker_state(ConfigDomain::Ui),
            Some(BreakerState::Open { .. })
        ));

        let r = w.observe(ConfigDomain::Policy, "managed", b"policy = \"strict\"", now, ok_validate(4));
        assert_eq!(
            r,
            WatchOutcome::Published { generation: 4 },
            "Policy publish must proceed despite UI breaker Open"
        );
    }

    #[test]
    fn lkg_recorded_per_domain() {
        // §12: each domain tracks its own last-known-good.
        let mut w = ConfigWatcher::default();
        let now = Instant::now();
        let _ = w.observe(ConfigDomain::Prompt, "user", b"prompt-a", now, ok_validate(2));
        let _ = w.observe(ConfigDomain::Sandbox, "system", b"profile-a", now, ok_validate(5));

        let prompt_lkg = w.last_known_good(ConfigDomain::Prompt).unwrap();
        assert_eq!(prompt_lkg.generation, 2);
        assert_eq!(prompt_lkg.content_hash, ConfigWatcher::content_hash(b"prompt-a"));
        assert_eq!(w.last_known_good(ConfigDomain::Sandbox).unwrap().generation, 5);
        assert!(w.last_known_good(ConfigDomain::Ui).is_none());
    }

    #[test]
    fn force_probe_never_skips_validation() {
        // §11.3: manual probe triggers one HalfOpen attempt but the
        // validation closure still runs (and failing content still fails).
        let cfg = PublishBreakerConfig {
            failure_threshold: 1,
            base_cooldown: Duration::from_secs(600),
            ..Default::default()
        };
        let mut w = ConfigWatcher::with_breaker_config(cfg);
        let now = Instant::now();
        let bad = b"[[[broken";

        let _ = w.observe(ConfigDomain::Root, "user", bad, now, fail_validate("e"));
        // Even force-probing the same bad content must re-validate and reject.
        let r = w.force_probe(ConfigDomain::Root, "user", bad, now, fail_validate("e"));
        assert!(matches!(r, WatchOutcome::Rejected { .. }));
        assert_eq!(w.compile_attempts(ConfigDomain::Root), 2);
    }
}
