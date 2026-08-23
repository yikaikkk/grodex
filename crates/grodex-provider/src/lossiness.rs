//! LossinessGate + LossinessManifest — explicit degradation encapsulation.
//!
//! Design Doc 14 (Lossiness Gate / acceptance #6/#10/#11/#12):
//! model switching and route failover must NEVER degrade silently. This
//! module combines [`CompatibilityGate`](crate::switch::CompatibilityGate)
//! (the 7-dimension check) with the route's DECLARED degradation policy
//! and the semantic commit fence into one structured, auditable artifact:
//!
//! - required-capability loss (tools / modality / compaction backend) →
//!   the switch is REJECTED (acceptance #11);
//! - optional-capability degradation (reasoning, parallel tool calls,
//!   context-window compaction) is allowed ONLY when the route explicitly
//!   declares it, and each allowed degradation produces a
//!   [`ModelCapabilityDegradedEvent`] (acceptance: "optional 降级会产生
//!   显式事件");
//! - everything lands in a [`LossinessManifest`] so UI/rollout can explain
//!   the original candidate, the target, the failure class, and the
//!   breaker/fence state (acceptance #12).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::switch::{CompatibilityVerdict, ModelSwitchPlan, SwitchReason};

/// Capability dimension names used in declared-degradation sets and
/// manifest entries. Keep in sync with `CompatibilityGate`'s dimensions.
pub mod dimension {
    pub const CONTEXT_WINDOW: &str = "context_window";
    pub const TOOL_SCHEMA: &str = "tool_schema";
    pub const PARALLEL_TOOL_CALLS: &str = "parallel_tool_calls";
    pub const REASONING: &str = "reasoning";
    pub const MODALITY: &str = "modality";
    pub const COMPACTION_BACKEND: &str = "compaction_backend";
}

/// How one capability dimension fares under the switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "class", content = "detail")]
pub enum LossinessClass {
    /// No loss on this dimension.
    Lossless,
    /// Degraded, but the route EXPLICITLY declared this degradation —
    /// allowed, and must emit a `ModelCapabilityDegraded` event.
    DeclaredDegradation,
    /// Degraded WITHOUT a route declaration — forbidden (no silent
    /// degradation). Rejects the switch.
    UndeclaredDegradation,
    /// A REQUIRED capability is missing on the target — never degradable.
    /// Rejects the switch (acceptance #11).
    RequiredLost,
}

impl LossinessClass {
    pub fn is_rejection(&self) -> bool {
        matches!(
            self,
            Self::UndeclaredDegradation | Self::RequiredLost
        )
    }
}

/// One dimension's outcome in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LossinessEntry {
    /// Dimension name (see [`dimension`]).
    pub dimension: String,
    pub class: LossinessClass,
    /// Human-readable loss descriptions from the compatibility verdict.
    pub losses: Vec<String>,
}

/// Explicit event describing one ALLOWED capability degradation
/// (Doc 14: "optional reasoning/parallel-tools 降级会产生显式事件").
/// Route producers emit this into rollout/UI when a manifest is adopted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilityDegradedEvent {
    /// Dimension that degraded (e.g. "reasoning").
    pub capability: String,
    /// Binding the Turn started on.
    pub from_binding_id: String,
    /// Target provider/model of the switch.
    pub to_provider_id: String,
    pub to_model_id: String,
    /// Why the switch happened (user request / failover / ...).
    pub reason: String,
    /// Concrete loss descriptions.
    pub losses: Vec<String>,
}

/// The full, auditable outcome of evaluating a model switch / failover
/// candidate (acceptance #6: allowed differences must land here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LossinessManifest {
    pub old_binding_id: String,
    pub new_provider_id: String,
    pub new_model_id: String,
    pub reason: SwitchReasonView,
    pub entries: Vec<LossinessEntry>,
    /// True only when no entry is a rejection. When false the candidate
    /// MUST be skipped / the switch refused.
    pub allowed: bool,
    /// Whether compaction must run before the switch can proceed.
    pub requires_compaction: bool,
    /// Whether the semantic commit fence was already crossed when this
    /// evaluation ran (transparent failover is then forbidden — Doc 14
    /// acceptance #10; recorded for rollout explainability).
    pub semantic_fence_crossed: bool,
}

/// Serializable mirror of [`SwitchReason`] (the original is not serde).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchReasonView {
    UserRequested,
    Failover,
    CostOptimization,
    CapabilityUpgrade,
    ProviderError,
}

impl From<&SwitchReason> for SwitchReasonView {
    fn from(r: &SwitchReason) -> Self {
        match r {
            SwitchReason::UserRequested => Self::UserRequested,
            SwitchReason::Failover => Self::Failover,
            SwitchReason::CostOptimization => Self::CostOptimization,
            SwitchReason::CapabilityUpgrade => Self::CapabilityUpgrade,
            SwitchReason::ProviderError => Self::ProviderError,
        }
    }
}

impl LossinessManifest {
    /// True when every dimension is lossless.
    pub fn is_lossless(&self) -> bool {
        self.entries
            .iter()
            .all(|e| e.class == LossinessClass::Lossless)
    }

    /// All allowed degradations → explicit events for rollout/UI.
    pub fn degradation_events(&self) -> Vec<ModelCapabilityDegradedEvent> {
        self.entries
            .iter()
            .filter(|e| e.class == LossinessClass::DeclaredDegradation)
            .map(|e| ModelCapabilityDegradedEvent {
                capability: e.dimension.clone(),
                from_binding_id: self.old_binding_id.clone(),
                to_provider_id: self.new_provider_id.clone(),
                to_model_id: self.new_model_id.clone(),
                reason: format!("{:?}", self.reason),
                losses: e.losses.clone(),
            })
            .collect()
    }

    /// All rejection reasons (for diagnostics / RouteEvent detail).
    pub fn rejection_reasons(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.class.is_rejection())
            .flat_map(|e| {
                let kind = match e.class {
                    LossinessClass::RequiredLost => "required capability lost",
                    LossinessClass::UndeclaredDegradation => "undeclared degradation",
                    _ => unreachable!("filtered by is_rejection"),
                };
                if e.losses.is_empty() {
                    vec![format!("{}: {}", e.dimension, kind)]
                } else {
                    e.losses
                        .iter()
                        .map(|l| format!("{}: {} ({})", e.dimension, kind, l))
                        .collect()
                }
            })
            .collect()
    }
}

/// The Lossiness Gate: CompatibilityGate verdicts + route-declared
/// degradation policy + semantic fence → structured manifest.
///
/// Constructed with the set of capability dimensions the route EXPLICITLY
/// allows degrading (Doc 14 §route: "optional capability 可以按 route
/// 显式声明降级"). Anything not declared fails closed.
#[derive(Debug, Clone, Default)]
pub struct LossinessGate {
    declared_degradations: BTreeSet<String>,
}

impl LossinessGate {
    /// No declared degradations — EVERY loss rejects (strictest policy).
    pub fn strict() -> Self {
        Self::default()
    }

    /// Route declares these dimensions degradable (e.g. "reasoning",
    /// "parallel_tool_calls").
    pub fn with_declared_degradations(dims: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            declared_degradations: dims.into_iter().map(Into::into).collect(),
        }
    }

    /// Evaluate a switch plan produced by
    /// [`CompatibilityGate::check_with_context`](crate::switch::CompatibilityGate).
    ///
    /// `semantic_fence_crossed`: whether the current Step already emitted
    /// semantic output. Transparent failover-style switches are forbidden
    /// past the fence (acceptance #10) — recorded in the manifest; the
    /// caller decides whether the reason (e.g. UserRequested) overrides.
    pub fn evaluate(
        &self,
        plan: &ModelSwitchPlan,
        semantic_fence_crossed: bool,
    ) -> LossinessManifest {
        let dims: [(&str, &CompatibilityVerdict); 6] = [
            (dimension::CONTEXT_WINDOW, &plan.context_fit),
            (dimension::TOOL_SCHEMA, &plan.tool_schema_compatibility),
            (
                dimension::PARALLEL_TOOL_CALLS,
                &plan.parallel_tool_call_compatibility,
            ),
            (dimension::REASONING, &plan.reasoning_compatibility),
            (dimension::MODALITY, &plan.modality_compatibility),
            (
                dimension::COMPACTION_BACKEND,
                &plan.compaction_backend_compatibility,
            ),
        ];

        let mut entries = Vec::with_capacity(dims.len());
        for (name, verdict) in dims {
            let entry = match verdict {
                CompatibilityVerdict::Compatible => LossinessEntry {
                    dimension: name.to_string(),
                    class: LossinessClass::Lossless,
                    losses: Vec::new(),
                },
                CompatibilityVerdict::Incompatible(reason) => LossinessEntry {
                    dimension: name.to_string(),
                    class: LossinessClass::RequiredLost,
                    losses: vec![reason.clone()],
                },
                CompatibilityVerdict::CompatibleWithLoss(losses) => {
                    let class = if self.declared_degradations.contains(name) {
                        LossinessClass::DeclaredDegradation
                    } else {
                        LossinessClass::UndeclaredDegradation
                    };
                    LossinessEntry {
                        dimension: name.to_string(),
                        class,
                        losses: losses.clone(),
                    }
                }
            };
            entries.push(entry);
        }

        let allowed = entries.iter().all(|e| !e.class.is_rejection());
        LossinessManifest {
            old_binding_id: plan.old_binding_id.clone(),
            new_provider_id: plan.new_provider_id.clone(),
            new_model_id: plan.new_model_id.clone(),
            reason: SwitchReasonView::from(&plan.reason),
            entries,
            allowed,
            requires_compaction: plan.requires_compaction,
            semantic_fence_crossed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::{ModelBinding, ReasoningPolicy};
    use crate::descriptor::{CompactionCapabilities, ModelDescriptor, WireProtocol};
    use crate::switch::{CompatibilityGate, SwitchContext};

    fn binding() -> ModelBinding {
        let mut b = ModelBinding::new(
            "openai".into(),
            1,
            "gpt-5".into(),
            1,
            WireProtocol::Responses,
        );
        b.reasoning_policy = ReasoningPolicy::Visible;
        b
    }

    fn descriptor(supports_reasoning: bool, supports_parallel: bool) -> ModelDescriptor {
        ModelDescriptor {
            model_id: "deepseek-chat".into(),
            provider_id: "deepseek".into(),
            wire_model_name: "deepseek-chat".into(),
            model_revision: 1,
            context_window: 64_000,
            max_output_tokens: 8_192,
            tokenizer_id: None,
            tokenizer_version: None,
            supports_tools: true,
            supports_parallel_tool_calls: supports_parallel,
            supports_reasoning,
            reasoning_modes: Vec::new(),
            supports_images: true,
            supports_prompt_cache: false,
            supports_structured_output: false,
            compaction_capabilities: CompactionCapabilities::None,
        }
    }

    #[test]
    fn lossless_switch_produces_empty_manifest_allowed() {
        let old = binding();
        let new = descriptor(true, true);
        let plan = CompatibilityGate::check_with_context(
            &old,
            &new,
            10_000,
            SwitchReason::Failover,
            &SwitchContext::default(),
        );
        let m = LossinessGate::strict().evaluate(&plan, false);
        assert!(m.allowed);
        assert!(m.is_lossless());
        assert!(m.degradation_events().is_empty());
    }

    #[test]
    fn undeclared_reasoning_loss_rejects_silently_no_more() {
        // Acceptance #11 / Lossiness Gate: visible reasoning lost on the
        // target without a route declaration MUST reject.
        let old = binding();
        let new = descriptor(false, true);
        let plan = CompatibilityGate::check_with_context(
            &old,
            &new,
            10_000,
            SwitchReason::Failover,
            &SwitchContext::default(),
        );
        let m = LossinessGate::strict().evaluate(&plan, false);
        assert!(!m.allowed);
        let reasons = m.rejection_reasons();
        assert!(reasons.iter().any(|r| r.contains("reasoning")));
        assert!(m.degradation_events().is_empty(), "no event without declaration");
    }

    #[test]
    fn declared_degradation_allowed_and_emits_event() {
        let old = binding();
        let new = descriptor(false, false);
        let ctx = SwitchContext {
            uses_parallel_tool_calls: true,
            ..Default::default()
        };
        let plan = CompatibilityGate::check_with_context(
            &old,
            &new,
            10_000,
            SwitchReason::Failover,
            &ctx,
        );
        let gate = LossinessGate::with_declared_degradations([
            dimension::REASONING,
            dimension::PARALLEL_TOOL_CALLS,
        ]);
        let m = gate.evaluate(&plan, false);
        assert!(m.allowed, "declared degradations must be admitted");
        assert!(!m.is_lossless());
        let events = m.degradation_events();
        assert_eq!(events.len(), 2, "one explicit event per degraded capability");
        assert!(events.iter().any(|e| e.capability == "reasoning"));
        assert!(events
            .iter()
            .any(|e| e.capability == "parallel_tool_calls"));
        assert_eq!(events[0].from_binding_id, old.binding_id.to_string());
    }

    #[test]
    fn required_tool_support_loss_always_rejects_even_if_declared() {
        // Declaring "tool_schema" degradable must NOT admit a target that
        // lacks tool support entirely — required capabilities are never
        // degradable (the gate reports Incompatible → RequiredLost).
        let old = binding();
        let mut new = descriptor(true, true);
        new.supports_tools = false;
        let plan = CompatibilityGate::check_with_context(
            &old,
            &new,
            10_000,
            SwitchReason::Failover,
            &SwitchContext::default(),
        );
        let gate = LossinessGate::with_declared_degradations([dimension::TOOL_SCHEMA]);
        let m = gate.evaluate(&plan, false);
        assert!(!m.allowed);
        assert!(m
            .rejection_reasons()
            .iter()
            .any(|r| r.contains("tool_schema")));
    }

    #[test]
    fn fence_state_recorded_for_rollout_explain() {
        let old = binding();
        let new = descriptor(true, true);
        let plan = CompatibilityGate::check_with_context(
            &old,
            &new,
            10_000,
            SwitchReason::Failover,
            &SwitchContext::default(),
        );
        let m = LossinessGate::strict().evaluate(&plan, true);
        assert!(m.semantic_fence_crossed, "fence state must be recorded");
        assert_eq!(m.reason, SwitchReasonView::Failover);
        // Serializable for rollout journaling.
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("semantic_fence_crossed"));
    }

    #[test]
    fn compaction_needs_declared_context_window_degradation() {
        // Context slightly over window → CompatibleWithLoss(compaction).
        // Undeclared → reject; declared → allowed with requires_compaction.
        let old = binding();
        let new = descriptor(true, true);
        let plan = CompatibilityGate::check_with_context(
            &old,
            &new,
            new.context_window + 10,
            SwitchReason::Failover,
            &SwitchContext::default(),
        );
        assert!(!LossinessGate::strict().evaluate(&plan, false).allowed);
        let gate =
            LossinessGate::with_declared_degradations([dimension::CONTEXT_WINDOW]);
        let m = gate.evaluate(&plan, false);
        assert!(m.allowed);
        assert!(m.requires_compaction);
    }
}
