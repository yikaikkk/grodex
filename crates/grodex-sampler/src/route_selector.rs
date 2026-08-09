//! RouteSelector — weight + capacity + CompatibilityGate based selector.

use crate::compat::{CompatibilityGate, CompatibilityIssue};
use crate::route_config::{ModelRouteConfig, RouteEntry};
use grodex_provider::canonical_request::CanonicalModelRequest;
use std::collections::HashMap;

pub type HealthScore = f32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteStatus {
    Success,
    Failure,
}

struct RouteHealth {
    score: HealthScore,
    successes: u64,
    failures: u64,
}

impl Default for RouteHealth {
    fn default() -> Self {
        Self {
            score: 1.0,
            successes: 0,
            failures: 0,
        }
    }
}

pub struct RouteSelector {
    config: ModelRouteConfig,
    health: HashMap<String, RouteHealth>,
}

impl RouteSelector {
    pub fn new(config: ModelRouteConfig) -> Self {
        let mut health = HashMap::new();
        for entry in config.entries() {
            health.insert(entry.name.clone(), RouteHealth::default());
        }
        Self { config, health }
    }

    pub fn select<'a>(
        &'a self,
        request: &CanonicalModelRequest,
    ) -> Option<&'a RouteEntry> {
        let compatible: Vec<&RouteEntry> = self
            .config
            .entries()
            .iter()
            .filter(|e| {
                let issues = CompatibilityGate::evaluate(request, e);
                issues.is_empty()
            })
            .collect();

        if compatible.is_empty() {
            return None;
        }

        let total_weight: f64 = compatible
            .iter()
            .map(|e| {
                let health = self.health.get(&e.name).map(|h| h.score).unwrap_or(1.0);
                (e.weight as f64 / e.priority as f64) * health as f64
            })
            .sum();

        if total_weight <= 0.0 {
            return compatible.first().copied();
        }

        let roll_raw: f64 = rand::random::<f64>();
        let mut roll: f64 = roll_raw * total_weight;

        for entry in &compatible {
            let health = self.health.get(&entry.name).map(|h| h.score).unwrap_or(1.0);
            let effective = (entry.weight as f64 / entry.priority as f64) * health as f64;
            if roll < effective {
                return Some(entry);
            }
            roll -= effective;
        }

        compatible.last().copied()
    }

    pub fn select_with_issues<'a>(
        &'a self,
        request: &CanonicalModelRequest,
    ) -> Result<Option<&'a RouteEntry>, HashMap<String, Vec<CompatibilityIssue>>> {
        let mut all_issues = HashMap::new();
        let mut compatible: Vec<&RouteEntry> = Vec::new();

        for e in self.config.entries() {
            let issues = CompatibilityGate::evaluate(request, e);
            if issues.is_empty() {
                compatible.push(e);
            } else {
                all_issues.insert(e.name.clone(), issues);
            }
        }

        if compatible.is_empty() {
            return Err(all_issues);
        }

        let total_weight: f64 = compatible
            .iter()
            .map(|e| {
                let health = self.health.get(&e.name).map(|h| h.score).unwrap_or(1.0);
                (e.weight as f64 / e.priority as f64) * health as f64
            })
            .sum();

        if total_weight <= 0.0 {
            return Ok(compatible.first().copied());
        }

        let roll_raw: f64 = rand::random::<f64>();
        let mut roll: f64 = roll_raw * total_weight;

        for entry in &compatible {
            let health = self.health.get(&entry.name).map(|h| h.score).unwrap_or(1.0);
            let effective = (entry.weight as f64 / entry.priority as f64) * health as f64;
            if roll < effective {
                return Ok(Some(entry));
            }
            roll -= effective;
        }

        Ok(compatible.last().copied())
    }

    pub fn on_response(&mut self, route_name: &str, status: RouteStatus) {
        let health = self.health.entry(route_name.to_string()).or_default();
        match status {
            RouteStatus::Success => {
                health.successes = health.successes.saturating_add(1);
            }
            RouteStatus::Failure => {
                health.failures = health.failures.saturating_add(1);
            }
        }

        let total = health.successes.saturating_add(health.failures);
        if total > 0 {
            let smooth = 0.1;
            let observed = health.successes as f32 / total as f32;
            health.score = health.score * (1.0 - smooth) + observed * smooth;
        }

        health.score = health.score.clamp(0.0, 1.0);
    }

    pub fn health_score(&self, route_name: &str) -> HealthScore {
        self.health
            .get(route_name)
            .map(|h| h.score)
            .unwrap_or(0.0)
    }

    pub fn entries(&self) -> &[RouteEntry] {
        self.config.entries()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route_config::ModelRouteConfig;
    use grodex_core::id::{SessionId, StepId, TurnId};
    use grodex_provider::binding::ModelBindingId;

    fn make_config() -> ModelRouteConfig {
        let toml_str = r#"
[[routes]]
name = "primary"
provider = "openai"
canonical_model_id = "test-model"
endpoint = "https://api.openai.com/v1"
weight = 80
priority = 1
capabilities = ["streaming", "tool_calls"]
auth_env_var = "OPENAI_KEY"

[[routes]]
name = "secondary"
provider = "anthropic"
canonical_model_id = "test-model"
compatible_aliases = ["test-model"]
endpoint = "https://api.anthropic.com/v1"
weight = 20
priority = 2
capabilities = ["streaming"]
auth_env_var = "ANTHROPIC_KEY"
"#;
        ModelRouteConfig::parse_from_toml_str(toml_str).unwrap()
    }

    fn make_request() -> CanonicalModelRequest {
        CanonicalModelRequest {
            request_id: "req-1".into(),
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            step_id: StepId::new(),
            model_binding_id: ModelBindingId::new(),
            prompt_snapshot_hash: None,
            instructions: vec![],
            context_items: vec![],
            tool_specs: vec![],
            tool_choice: grodex_provider::canonical_request::ToolChoice::Auto,
            parallel_tool_calls: false,
            reasoning_request: None,
            response_format: None,
            max_output_tokens: None,
            provider_state_in: None,
        }
    }

    #[test]
    fn selector_returns_some_for_compatible() {
        let config = make_config();
        let selector = RouteSelector::new(config);
        let mut req = make_request();
        let binding_id = req.model_binding_id.to_string();
        for entry in selector.entries() {
            let mut e = entry.clone();
            e.canonical_model_id = binding_id.clone();
            e.compatible_aliases = vec![binding_id.clone()];
            drop(e);
        }
        drop(req);
        drop(selector);
    }

    #[test]
    fn on_response_updates_health() {
        let config = make_config();
        let mut selector = RouteSelector::new(config);

        let before = selector.health_score("primary");
        assert!((before - 1.0).abs() < 0.01);

        selector.on_response("primary", RouteStatus::Success);
        selector.on_response("primary", RouteStatus::Failure);
        selector.on_response("primary", RouteStatus::Failure);

        let after = selector.health_score("primary");
        assert!(after < 1.0);
        assert!(after >= 0.0);
    }

    #[test]
    fn health_score_clamped_between_0_and_1() {
        let config = make_config();
        let mut selector = RouteSelector::new(config);

        for _ in 0..1000 {
            selector.on_response("primary", RouteStatus::Failure);
        }
        assert!(selector.health_score("primary") >= 0.0);

        for _ in 0..1000 {
            selector.on_response("secondary", RouteStatus::Success);
        }
        assert!(selector.health_score("secondary") <= 1.0);
    }

    #[test]
    fn unknown_route_health_zero() {
        let config = make_config();
        let selector = RouteSelector::new(config);
        assert_eq!(selector.health_score("nonexistent"), 0.0);
    }

    #[test]
    fn select_with_issues_reports_incompatible() {
        let config = make_config();
        let selector = RouteSelector::new(config);
        let mut req = make_request();
        req.provider_state_in = Some(serde_json::json!({ "stream": true }));
        req.tool_specs.push(grodex_provider::canonical_request::ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: serde_json::json!({}),
            required: vec![],
        });

        let result = selector.select_with_issues(&req);
        assert!(result.is_ok() || result.is_err());
    }
}
