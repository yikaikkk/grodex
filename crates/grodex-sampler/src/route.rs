//! ModelRoute — ordered provider/model candidates with failover.
//!
//! Following Design Doc 14 §13.4: when a candidate fails with a
//! failover-eligible error (Transport, 5xx, RateLimited), the route
//! manager tries the next candidate. Each candidate has its own
//! CircuitBreaker, and the route has a shared budget.

use crate::breaker::{BreakerConfig, CircuitBreaker, Outcome};
use crate::retry::RetryBudget;
use grodex_provider::binding::ModelBinding;
use grodex_provider::descriptor::WireProtocol;
use grodex_provider::lossiness::{LossinessGate, LossinessManifest, SwitchReasonView};
use std::sync::Arc;
use std::time::Duration;

/// Sticky scope for turn affinity.
///
/// Controls how long the route stays "stuck" to a candidate after
/// a successful sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StickyScope {
    /// Reset to highest priority every Turn (current behaviour).
    #[default]
    Turn,
    /// Stay on the same candidate across Steps within a Turn.
    Step,
    /// Stay on the same candidate for the entire Session.
    Session,
}

/// Route-level attempt budget — independent from per-candidate RetryBudget.
///
/// Limits how many candidates are tried in one failover sequence and
/// the total wall-clock time spent on failover before giving up.
#[derive(Debug, Clone)]
pub struct RouteAttemptBudget {
    /// Max candidates to try in one failover sequence.
    pub max_candidates: usize,
    /// Max total wall-clock time for a single Turn's sampling.
    pub turn_deadline: Duration,
    /// Shared per-candidate retry budget (retries within one candidate).
    pub per_candidate: RetryBudget,
}

impl Default for RouteAttemptBudget {
    fn default() -> Self {
        Self {
            max_candidates: 5,
            turn_deadline: Duration::from_secs(120),
            per_candidate: RetryBudget::default(),
        }
    }
}

/// Routing observability event. Emitted at key failover decision points.
#[derive(Debug, Clone)]
pub enum RouteEvent {
    /// A candidate was selected for sampling.
    CandidateSelected { candidate_id: String, priority: u32 },
    /// A candidate succeeded and the route sticks to it.
    CandidateSucceeded { candidate_id: String },
    /// A candidate failed. `failover` indicates whether the route will
    /// try the next candidate.
    CandidateFailed { candidate_id: String, failover: bool },
    /// A candidate was REJECTED by the Lossiness Gate during failover
    /// (required capability lost or undeclared degradation — Doc 14
    /// acceptance #11: no silent degradation). `reasons` comes from the
    /// manifest's rejection entries.
    CandidateRejected { candidate_id: String, reasons: Vec<String> },
    /// All candidates exhausted — the route cannot serve this Turn.
    RouteExhausted { attempts: u32 },
    /// A candidate's breaker transitioned to Open.
    BreakerOpened { candidate_id: String },
}

/// One candidate in a ModelRoute.
#[derive(Clone)]
pub struct ModelCandidate {
    /// Unique candidate id.
    pub candidate_id: String,
    /// Provider identifier.
    pub provider_id: String,
    /// Model identifier.
    pub model_id: String,
    /// Wire protocol.
    pub wire_protocol: WireProtocol,
    /// Endpoint URL.
    pub endpoint: String,
    /// Priority (lower = tried first).
    pub priority: u32,
    /// Account ID for credential routing (Design Doc 14 §13.4).
    pub account_id: Option<String>,
    /// Deployment region (e.g. "us-east-1").
    pub region: Option<String>,
    /// Capability revision tag (for compatibility gating).
    pub revision: Option<String>,
    /// Circuit breaker for this candidate.
    pub breaker: Arc<CircuitBreaker>,
}

impl ModelCandidate {
    /// Build a ModelBinding from this candidate.
    pub fn binding(&self) -> ModelBinding {
        ModelBinding::new(
            self.provider_id.clone(),
            1,
            self.model_id.clone(),
            1,
            self.wire_protocol,
        )
    }
}

/// Ordered route of model candidates.
///
/// Candidates are tried in priority order (lowest first).
/// Once a candidate succeeds, the route "sticks" to it for the
/// remainder of the sticky scope. The next scope cycle starts
/// from the highest priority again.
#[derive(Clone)]
pub struct ModelRoute {
    /// Candidates sorted by priority.
    candidates: Vec<ModelCandidate>,
    /// Route-level attempt budget.
    budget: RouteAttemptBudget,
    /// Sticky scope controlling when the route resets.
    sticky_scope: StickyScope,
    /// Capability dimensions this route EXPLICITLY declares degradable
    /// (Doc 14: "optional capability 可以按 route 显式声明降级").
    /// Used to build the LossinessGate for failover gating.
    declared_degradations: Vec<String>,
    /// Total attempts across all candidates this Turn.
    turn_attempts: u32,
    /// Index of the currently selected candidate (-1 if none).
    current_index: i32,
    /// Pending events waiting to be drained by the caller.
    pending_events: Vec<RouteEvent>,
}

impl ModelRoute {
    /// Create a new route from candidates with default budget and Turn stickiness.
    pub fn new(candidates: Vec<ModelCandidate>, budget: RetryBudget) -> Self {
        Self::with_budget(
            candidates,
            RouteAttemptBudget {
                per_candidate: budget,
                ..Default::default()
            },
        )
    }

    /// Create a new route with an explicit RouteAttemptBudget and default stickiness.
    pub fn with_budget(candidates: Vec<ModelCandidate>, budget: RouteAttemptBudget) -> Self {
        Self::with_config(candidates, budget, StickyScope::Turn)
    }

    /// Create a new route with full configuration.
    pub fn with_config(
        candidates: Vec<ModelCandidate>,
        budget: RouteAttemptBudget,
        sticky_scope: StickyScope,
    ) -> Self {
        let mut sorted = candidates;
        sorted.sort_by_key(|c| c.priority);
        Self {
            candidates: sorted,
            budget,
            sticky_scope,
            declared_degradations: Vec::new(),
            turn_attempts: 0,
            current_index: -1,
            pending_events: Vec::new(),
        }
    }

    /// Declare capability dimensions this route allows degrading during
    /// failover (e.g. "reasoning", "parallel_tool_calls"). Anything not
    /// declared fails closed in the Lossiness Gate.
    pub fn with_declared_degradations(mut self, dims: impl IntoIterator<Item = String>) -> Self {
        self.declared_degradations = dims.into_iter().collect();
        self
    }

    /// The route's declared degradation policy (for audit/rollout).
    pub fn declared_degradations(&self) -> &[String] {
        &self.declared_degradations
    }

    /// Build a LossinessGate from this route's declared degradation policy.
    pub fn lossiness_gate(&self) -> LossinessGate {
        LossinessGate::with_declared_degradations(self.declared_degradations.clone())
    }

    /// Get the current candidate (the one we're "stuck" to).
    pub fn current(&self) -> Option<&ModelCandidate> {
        if self.current_index >= 0 {
            self.candidates.get(self.current_index as usize)
        } else {
            None
        }
    }

    /// Select the first available candidate. Called at Turn start.
    /// Skips candidates whose breaker is open.
    /// Returns the selected candidate and its ModelBinding.
    pub fn select_first(&mut self) -> Option<(&ModelCandidate, ModelBinding)> {
        // Session stickiness: keep the current candidate if still healthy.
        if self.sticky_scope == StickyScope::Session && self.current_index >= 0 {
            if let Some(c) = self.candidates.get(self.current_index as usize) {
                if c.breaker.check().is_ok() {
                    self.turn_attempts = 0;
                    return Some((c, c.binding()));
                }
            }
        }

        for (i, candidate) in self.candidates.iter().enumerate() {
            if candidate.breaker.check().is_ok() {
                self.current_index = i as i32;
                self.turn_attempts = 0;
                self.pending_events.push(RouteEvent::CandidateSelected {
                    candidate_id: candidate.candidate_id.clone(),
                    priority: candidate.priority,
                });
                let binding = candidate.binding();
                return Some((candidate, binding));
            } else if candidate.breaker.is_open() {
                self.pending_events.push(RouteEvent::BreakerOpened {
                    candidate_id: candidate.candidate_id.clone(),
                });
            }
        }
        None // all breakers open
    }

    /// Try the next candidate after a failover-eligible failure.
    /// Returns the next candidate and binding, or None if exhausted.
    pub fn try_next(&mut self) -> Option<(&ModelCandidate, ModelBinding)> {
        let start = (self.current_index + 1) as usize;
        let candidates_tried = start;
        if candidates_tried >= self.budget.max_candidates {
            self.pending_events.push(RouteEvent::RouteExhausted {
                attempts: self.turn_attempts,
            });
            return None;
        }
        for i in start..self.candidates.len() {
            if self.candidates[i].breaker.check().is_ok() {
                self.current_index = i as i32;
                self.pending_events.push(RouteEvent::CandidateSelected {
                    candidate_id: self.candidates[i].candidate_id.clone(),
                    priority: self.candidates[i].priority,
                });
                let binding = self.candidates[i].binding();
                return Some((&self.candidates[i], binding));
            }
        }
        self.pending_events.push(RouteEvent::RouteExhausted {
            attempts: self.turn_attempts,
        });
        None
    }

    /// Lossiness-gated failover (Doc 14 acceptance #6/#10/#11/#12).
    ///
    /// Like [`Self::try_next`], but each breaker-healthy candidate is
    /// first evaluated by `evaluate`, which returns the candidate's
    /// [`LossinessManifest`] (built by the caller from the current
    /// binding + the candidate's ModelDescriptor via the route's
    /// [`Self::lossiness_gate`]). A candidate is rejected when:
    ///
    /// - the manifest is not allowed (required capability lost or
    ///   undeclared degradation), or
    /// - the semantic fence was already crossed AND the switch is a
    ///   transparent failover (Doc 14 acceptance #10).
    ///
    /// Rejections emit `RouteEvent::CandidateRejected` with the manifest
    /// reasons so rollout/UI can explain why the candidate was skipped.
    /// Returns the first admitted candidate together with its manifest.
    pub fn try_next_lossiness(
        &mut self,
        mut evaluate: impl FnMut(&ModelCandidate) -> LossinessManifest,
    ) -> Option<(&ModelCandidate, ModelBinding, LossinessManifest)> {
        let start = (self.current_index + 1) as usize;
        if start >= self.budget.max_candidates {
            self.pending_events.push(RouteEvent::RouteExhausted {
                attempts: self.turn_attempts,
            });
            return None;
        }
        for i in start..self.candidates.len() {
            if !self.candidates[i].breaker.check().is_ok() {
                continue;
            }
            let manifest = evaluate(&self.candidates[i]);
            // Transparent failover past the semantic fence is forbidden.
            let fence_blocks = manifest.semantic_fence_crossed
                && manifest.reason == SwitchReasonView::Failover;
            if manifest.allowed && !fence_blocks {
                self.current_index = i as i32;
                self.pending_events.push(RouteEvent::CandidateSelected {
                    candidate_id: self.candidates[i].candidate_id.clone(),
                    priority: self.candidates[i].priority,
                });
                let binding = self.candidates[i].binding();
                return Some((&self.candidates[i], binding, manifest));
            }
            let mut reasons = manifest.rejection_reasons();
            if fence_blocks {
                reasons.push(
                    "semantic fence already crossed; transparent failover forbidden".to_string(),
                );
            }
            self.pending_events.push(RouteEvent::CandidateRejected {
                candidate_id: self.candidates[i].candidate_id.clone(),
                reasons,
            });
        }
        self.pending_events.push(RouteEvent::RouteExhausted {
            attempts: self.turn_attempts,
        });
        None
    }

    /// Record a successful attempt on the current candidate.
    pub fn record_success(&mut self) {
        self.turn_attempts += 1;
        if let Some(c) = self.current() {
            c.breaker.record(Outcome::Success);
            self.pending_events.push(RouteEvent::CandidateSucceeded {
                candidate_id: c.candidate_id.clone(),
            });
        }
    }

    /// Record a failed attempt. Returns true if failover should be attempted.
    pub fn record_failure(&mut self, is_failover_eligible: bool) -> bool {
        self.turn_attempts += 1;
        let candidate_id = self.current().map(|c| c.candidate_id.clone());
        if let Some(c) = self.current() {
            c.breaker.record(Outcome::Failure);
            if c.breaker.is_open() {
                if let Some(ref id) = candidate_id {
                    self.pending_events.push(RouteEvent::BreakerOpened {
                        candidate_id: id.clone(),
                    });
                }
            }
        }
        let failover = is_failover_eligible && self.turn_attempts < self.budget.per_candidate.max_attempts;
        if let Some(ref id) = candidate_id {
            self.pending_events.push(RouteEvent::CandidateFailed {
                candidate_id: id.clone(),
                failover,
            });
        }
        failover
    }

    /// Reset for a new Turn (go back to highest priority), unless
    /// the sticky scope is Session (keep the current candidate).
    pub fn reset_for_turn(&mut self) {
        if self.sticky_scope == StickyScope::Session {
            // Session stickiness: keep current selection, only reset attempts.
            self.turn_attempts = 0;
        } else {
            self.current_index = -1;
            self.turn_attempts = 0;
        }
    }

    /// Number of candidates.
    pub fn len(&self) -> usize {
        self.candidates.len()
    }

    /// Whether the route is empty.
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Drain pending routing events. The caller should persist these
    /// to the rollout journal for observability.
    pub fn drain_events(&mut self) -> Vec<RouteEvent> {
        std::mem::take(&mut self.pending_events)
    }
}

// ── TOML configuration parsing ─────────────────────────────────────

/// TOML representation of a single model route candidate.
///
/// ```toml
/// [[model_routes.default.candidates]]
/// candidate_id = "openai-primary"
/// provider_id = "openai"
/// model_id = "gpt-5"
/// wire_protocol = "responses"
/// endpoint = "https://api.openai.com/v1"
/// priority = 0
/// account_id = "default"
/// region = "us-east-1"
/// revision = "2024-08"
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CandidateToml {
    pub candidate_id: String,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default = "default_wire_protocol")]
    pub wire_protocol: String,
    pub endpoint: String,
    #[serde(default)]
    pub priority: u32,
    pub account_id: Option<String>,
    pub region: Option<String>,
    pub revision: Option<String>,
}

/// TOML representation of a model route.
///
/// ```toml
/// [model_routes.default]
/// sticky_scope = "turn"
/// max_candidates = 5
/// turn_deadline_secs = 120
/// allowed_degradations = ["reasoning", "parallel_tool_calls"]
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelRouteToml {
    #[serde(default = "default_sticky_scope")]
    pub sticky_scope: String,
    #[serde(default = "default_max_candidates")]
    pub max_candidates: usize,
    #[serde(default = "default_turn_deadline_secs")]
    pub turn_deadline_secs: u64,
    /// Capability dimensions this route explicitly allows degrading on
    /// failover (Doc 14 Lossiness Gate). Undeclared losses fail closed.
    #[serde(default)]
    pub allowed_degradations: Vec<String>,
    pub candidates: Vec<CandidateToml>,
}

fn default_wire_protocol() -> String {
    "responses".to_string()
}
fn default_sticky_scope() -> String {
    "turn".to_string()
}
fn default_max_candidates() -> usize {
    5
}
fn default_turn_deadline_secs() -> u64 {
    120
}

impl ModelRouteToml {
    /// Parse a `[model_routes]` section from a TOML value.
    ///
    /// Looks for `model_routes.<route_name>` where `<route_name>` is
    /// typically "default". Returns `None` if the section is absent.
    pub fn from_config(config: &toml::Value, route_name: &str) -> Option<Self> {
        let table = config.as_table()?;
        let routes = table.get("model_routes")?.as_table()?;
        let route = routes.get(route_name)?;
        route.clone().try_into().ok()
    }

    /// Build a `ModelRoute` from this TOML config.
    pub fn to_model_route(&self) -> ModelRoute {
        let candidates: Vec<ModelCandidate> = self
            .candidates
            .iter()
            .map(|c| ModelCandidate {
                candidate_id: c.candidate_id.clone(),
                provider_id: c.provider_id.clone(),
                model_id: c.model_id.clone(),
                wire_protocol: parse_wire_protocol(&c.wire_protocol),
                endpoint: c.endpoint.clone(),
                priority: c.priority,
                account_id: c.account_id.clone(),
                region: c.region.clone(),
                revision: c.revision.clone(),
                breaker: Arc::new(CircuitBreaker::new(BreakerConfig::default())),
            })
            .collect();

        let sticky_scope = match self.sticky_scope.as_str() {
            "step" => StickyScope::Step,
            "session" => StickyScope::Session,
            _ => StickyScope::Turn,
        };

        let budget = RouteAttemptBudget {
            max_candidates: self.max_candidates,
            turn_deadline: Duration::from_secs(self.turn_deadline_secs),
            per_candidate: RetryBudget::default(),
        };

        ModelRoute::with_config(candidates, budget, sticky_scope)
            .with_declared_degradations(self.allowed_degradations.clone())
    }
}

fn parse_wire_protocol(s: &str) -> WireProtocol {
    match s.to_lowercase().as_str() {
        "chat" | "chat_completions" => WireProtocol::ChatCompletions,
        "messages" => WireProtocol::Messages,
        _ => WireProtocol::Responses,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::BreakerConfig;

    fn make_candidate(id: &str, priority: u32) -> ModelCandidate {
        ModelCandidate {
            candidate_id: id.into(),
            provider_id: format!("provider-{id}"),
            model_id: format!("model-{id}"),
            wire_protocol: WireProtocol::Responses,
            endpoint: format!("https://api.{id}.com/v1"),
            priority,
            account_id: None,
            region: None,
            revision: None,
            breaker: Arc::new(CircuitBreaker::new(BreakerConfig::default())),
        }
    }

    #[test]
    fn select_first_available() {
        let candidates = vec![
            make_candidate("primary", 0),
            make_candidate("secondary", 1),
        ];
        let mut route = ModelRoute::new(candidates, RetryBudget::default());

        let (c, _) = route.select_first().unwrap();
        assert_eq!(c.candidate_id, "primary");
    }

    #[test]
    fn failover_to_next() {
        let candidates = vec![
            make_candidate("primary", 0),
            make_candidate("secondary", 1),
        ];
        let mut route = ModelRoute::new(candidates, RetryBudget::default());

        route.select_first().unwrap();
        route.record_failure(true);

        let (c, _) = route.try_next().unwrap();
        assert_eq!(c.candidate_id, "secondary");
    }

    #[test]
    fn skips_open_breakers() {
        let mut candidates = vec![
            make_candidate("primary", 0),
            make_candidate("secondary", 1),
        ];
        // Trip primary's breaker.
        for _ in 0..20 {
            candidates[0].breaker.record(Outcome::Failure);
        }
        assert!(candidates[0].breaker.is_open());

        let mut route = ModelRoute::new(candidates, RetryBudget::default());
        let (c, _) = route.select_first().unwrap();
        assert_eq!(c.candidate_id, "secondary", "should skip open primary");
    }

    #[test]
    fn route_budget_exhausted() {
        let candidates = vec![
            make_candidate("primary", 0),
            make_candidate("secondary", 1),
        ];
        let budget = RouteAttemptBudget {
            max_candidates: 1, // only 1 candidate allowed
            ..Default::default()
        };
        let mut route = ModelRoute::with_budget(candidates, budget);

        route.select_first().unwrap();
        route.record_failure(true);
        assert!(route.try_next().is_none(), "max_candidates budget exhausted");
    }

    #[test]
    fn session_stickiness_keeps_candidate_across_turns() {
        let candidates = vec![
            make_candidate("primary", 0),
            make_candidate("secondary", 1),
        ];
        let budget = RouteAttemptBudget::default();
        let mut route = ModelRoute::with_config(candidates, budget, StickyScope::Session);

        // First turn: select primary.
        route.select_first().unwrap();
        route.record_success();
        route.reset_for_turn();

        // Second turn: should still be on primary (Session stickiness).
        let (c, _) = route.select_first().unwrap();
        assert_eq!(c.candidate_id, "primary", "session stickiness should keep primary");
    }

    #[test]
    fn turn_stickiness_resets_each_turn() {
        let candidates = vec![
            make_candidate("primary", 0),
            make_candidate("secondary", 1),
        ];
        let budget = RouteAttemptBudget::default();
        let mut route = ModelRoute::with_config(candidates, budget, StickyScope::Turn);

        route.select_first().unwrap();
        route.record_success();
        route.reset_for_turn();

        // Turn stickiness: should re-select from highest priority.
        let (c, _) = route.select_first().unwrap();
        assert_eq!(c.candidate_id, "primary");
    }

    #[test]
    fn max_candidates_budget_limits_failover() {
        let candidates = vec![
            make_candidate("a", 0),
            make_candidate("b", 1),
            make_candidate("c", 2),
        ];
        let budget = RouteAttemptBudget {
            max_candidates: 2,
            ..Default::default()
        };
        let mut route = ModelRoute::with_budget(candidates, budget);

        route.select_first().unwrap(); // candidate "a"
        route.record_failure(true);
        assert!(route.try_next().is_some()); // candidate "b" (2nd candidate)
        route.record_failure(true);
        // max_candidates=2 → should not try "c" (3rd candidate)
        assert!(route.try_next().is_none(), "max_candidates budget exhausted");
    }

    #[test]
    fn routing_events_emitted_and_drainable() {
        let candidates = vec![
            make_candidate("primary", 0),
            make_candidate("secondary", 1),
        ];
        let mut route = ModelRoute::with_budget(candidates, RouteAttemptBudget::default());

        route.select_first().unwrap();
        route.record_failure(true);
        route.try_next().unwrap();
        route.record_success();

        let events = route.drain_events();
        assert!(!events.is_empty(), "should have emitted events");

        // Expected sequence: CandidateSelected(primary) → CandidateFailed(primary) →
        // CandidateSelected(secondary) → CandidateSucceeded(secondary)
        assert!(events.iter().any(|e| matches!(e, RouteEvent::CandidateSelected { candidate_id, .. } if candidate_id == "primary")));
        assert!(events.iter().any(|e| matches!(e, RouteEvent::CandidateFailed { failover: true, .. })));
        assert!(events.iter().any(|e| matches!(e, RouteEvent::CandidateSucceeded { .. })));

        // Drain should clear.
        assert!(route.drain_events().is_empty(), "events should be drained");
    }

    fn manifest_for(candidate: &ModelCandidate, allowed: bool, fence: bool) -> LossinessManifest {
        use grodex_provider::lossiness::{LossinessClass, LossinessEntry};
        let entries = if allowed {
            vec![LossinessEntry {
                dimension: grodex_provider::lossiness::dimension::REASONING.into(),
                class: LossinessClass::Lossless,
                losses: Vec::new(),
            }]
        } else {
            vec![LossinessEntry {
                dimension: grodex_provider::lossiness::dimension::REASONING.into(),
                class: LossinessClass::UndeclaredDegradation,
                losses: vec!["visible reasoning not supported".into()],
            }]
        };
        LossinessManifest {
            old_binding_id: "openai:1:gpt-5:1".into(),
            new_provider_id: candidate.provider_id.clone(),
            new_model_id: candidate.model_id.clone(),
            reason: SwitchReasonView::Failover,
            entries,
            allowed,
            requires_compaction: false,
            semantic_fence_crossed: fence,
        }
    }

    #[test]
    fn lossiness_gate_rejects_undeclared_degradation_candidate() {
        let candidates = vec![
            make_candidate("primary", 0),
            make_candidate("lossy", 1),    // undeclared reasoning loss
            make_candidate("safe", 2),     // lossless
        ];
        let mut route = ModelRoute::new(candidates, RetryBudget::default());
        route.select_first().unwrap();
        route.record_failure(true);

        let (c, _, manifest) = route
            .try_next_lossiness(|cand| manifest_for(cand, cand.candidate_id != "lossy", false))
            .unwrap();
        assert_eq!(c.candidate_id, "safe", "lossy candidate must be skipped");
        assert!(manifest.allowed);

        let events = route.drain_events();
        assert!(
            events.iter().any(|e| matches!(
                e, RouteEvent::CandidateRejected { candidate_id, reasons }
                if candidate_id == "lossy" && reasons.iter().any(|r| r.contains("reasoning"))
            )),
            "rejection must be explained via CandidateRejected"
        );
    }

    #[test]
    fn semantic_fence_blocks_transparent_failover() {
        let candidates = vec![
            make_candidate("primary", 0),
            make_candidate("secondary", 1),
        ];
        let mut route = ModelRoute::new(candidates, RetryBudget::default());
        route.select_first().unwrap();
        route.record_failure(true);

        // Fence crossed + Failover reason → transparent failover forbidden.
        let res = route.try_next_lossiness(|cand| manifest_for(cand, true, true));
        assert!(res.is_none(), "fence must block transparent failover");
        let events = route.drain_events();
        assert!(
            events.iter().any(|e| matches!(
                e, RouteEvent::CandidateRejected { reasons, .. }
                if reasons.iter().any(|r| r.contains("semantic fence"))
            )),
            "fence rejection must be recorded for rollout explainability"
        );
    }

    #[test]
    fn route_exhausted_event_emitted() {
        let candidates = vec![
            make_candidate("a", 0),
        ];
        let budget = RouteAttemptBudget {
            max_candidates: 1,
            ..Default::default()
        };
        let mut route = ModelRoute::with_budget(candidates, budget);

        route.select_first().unwrap();
        route.record_failure(true);
        assert!(route.try_next().is_none());

        let events = route.drain_events();
        assert!(events.iter().any(|e| matches!(e, RouteEvent::RouteExhausted { .. })));
    }

    #[test]
    fn toml_parses_multi_candidate_route() {
        let toml_str = r#"
[model_routes.default]
sticky_scope = "session"
max_candidates = 3
turn_deadline_secs = 90

[[model_routes.default.candidates]]
candidate_id = "openai-primary"
provider_id = "openai"
model_id = "gpt-5"
wire_protocol = "responses"
endpoint = "https://api.openai.com/v1"
priority = 0
account_id = "default"
region = "us-east-1"

[[model_routes.default.candidates]]
candidate_id = "anthropic-fallback"
provider_id = "anthropic"
model_id = "claude-3-opus"
wire_protocol = "messages"
endpoint = "https://api.anthropic.com/v1"
priority = 1
"#;
        let config: toml::Value = toml::from_str(toml_str).unwrap();
        let route_toml = ModelRouteToml::from_config(&config, "default")
            .expect("should parse model_routes.default");
        let mut route = route_toml.to_model_route();

        assert_eq!(route.len(), 2);
        let (c, _) = route.select_first().unwrap();
        assert_eq!(c.candidate_id, "openai-primary");
        assert_eq!(c.account_id.as_deref(), Some("default"));
        assert_eq!(c.region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn toml_returns_none_when_no_routes() {
        let config: toml::Value = toml::from_str(r#"model = "gpt-5""#).unwrap();
        assert!(ModelRouteToml::from_config(&config, "default").is_none());
    }
}
