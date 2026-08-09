//! ModelSwitchPlan + CompatibilityGate — safe model switching.
//!
//! Design Doc 14 §14: switching models is not just changing a name string.
//! It requires checking context fit, tool schema compatibility,
//! reasoning continuity, modality support, and provider state clearing.

use crate::binding::ModelBinding;
use crate::descriptor::ModelDescriptor;

/// A plan for switching from one model to another.
#[derive(Debug, Clone)]
pub struct ModelSwitchPlan {
    pub old_binding_id: String,
    pub new_model_id: String,
    pub new_provider_id: String,
    pub reason: SwitchReason,
    /// 1. Whether the new model can fit the current context.
    pub context_fit: CompatibilityVerdict,
    /// 2. Whether tool schemas are compatible.
    pub tool_schema_compatibility: CompatibilityVerdict,
    /// 3. Whether parallel tool calls are compatible.
    pub parallel_tool_call_compatibility: CompatibilityVerdict,
    /// 4. Whether reasoning is compatible.
    pub reasoning_compatibility: CompatibilityVerdict,
    /// 5. Whether modalities (images, audio) are compatible.
    pub modality_compatibility: CompatibilityVerdict,
    /// 6. Whether the compaction backend is compatible.
    pub compaction_backend_compatibility: CompatibilityVerdict,
    /// Whether provider state must be cleared.
    pub provider_state_action: ProviderStateAction,
    /// Whether a compaction is required before switching.
    pub requires_compaction: bool,
    /// Any lossy mappings that will occur.
    pub lossy_mappings: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum SwitchReason {
    UserRequested,
    Failover,
    CostOptimization,
    CapabilityUpgrade,
    ProviderError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatibilityVerdict {
    Compatible,
    CompatibleWithLoss(Vec<String>),
    Incompatible(String),
}

impl CompatibilityVerdict {
    pub fn is_compatible(&self) -> bool {
        matches!(self, Self::Compatible | Self::CompatibleWithLoss(_))
    }

    pub fn is_fully_compatible(&self) -> bool {
        matches!(self, Self::Compatible)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStateAction {
    Keep,
    Clear,
    ClearWithWarning,
}

/// Validates whether a model switch is safe.
pub struct CompatibilityGate;

/// Extra context about the current session needed for compatibility checks.
#[derive(Debug, Clone, Default)]
pub struct SwitchContext {
    /// Whether the current session uses parallel tool calls.
    pub uses_parallel_tool_calls: bool,
    /// Whether the current context contains image inputs.
    pub has_images: bool,
    /// Whether the session relies on compaction (context is near the window limit).
    pub needs_compaction_backend: bool,
}

impl CompatibilityGate {
    /// Check all 7 compatibility dimensions and produce a switch plan.
    ///
    /// The 7 dimensions (Design Doc 14 §14):
    /// 1. Context fit (token window)
    /// 2. Tool schema support
    /// 3. Parallel tool call support
    /// 4. Reasoning continuity
    /// 5. Modality (images/audio)
    /// 6. Compaction backend
    /// 7. Provider state (cross-provider switch)
    pub fn check(
        old_binding: &ModelBinding,
        new_model: &ModelDescriptor,
        current_context_tokens: u64,
        reason: SwitchReason,
    ) -> ModelSwitchPlan {
        Self::check_with_context(old_binding, new_model, current_context_tokens, reason, &SwitchContext::default())
    }

    /// Full check with session context (parallel tools, images, compaction needs).
    pub fn check_with_context(
        old_binding: &ModelBinding,
        new_model: &ModelDescriptor,
        current_context_tokens: u64,
        reason: SwitchReason,
        ctx: &SwitchContext,
    ) -> ModelSwitchPlan {
        let context_fit = Self::check_context_fit(new_model, current_context_tokens);
        let tool_schema_compatibility = Self::check_tool_schema(old_binding, new_model);
        let parallel_tool_call_compatibility =
            Self::check_parallel_tool_calls(ctx.uses_parallel_tool_calls, new_model);
        let reasoning_compatibility = Self::check_reasoning(old_binding, new_model);
        let modality_compatibility = Self::check_modality(ctx.has_images, new_model);
        let compaction_backend_compatibility =
            Self::check_compaction_backend(ctx.needs_compaction_backend, new_model);
        let provider_state_action = Self::determine_provider_state(old_binding, new_model);

        let requires_compaction = !context_fit.is_fully_compatible()
            && context_fit.is_compatible(); // can fix with compaction

        let mut lossy_mappings = Vec::new();
        if let CompatibilityVerdict::CompatibleWithLoss(ref losses) = tool_schema_compatibility {
            lossy_mappings.extend(losses.clone());
        }
        if let CompatibilityVerdict::CompatibleWithLoss(ref losses) = parallel_tool_call_compatibility {
            lossy_mappings.extend(losses.clone());
        }
        if let CompatibilityVerdict::CompatibleWithLoss(ref losses) = reasoning_compatibility {
            lossy_mappings.extend(losses.clone());
        }
        if let CompatibilityVerdict::CompatibleWithLoss(ref losses) = modality_compatibility {
            lossy_mappings.extend(losses.clone());
        }

        ModelSwitchPlan {
            old_binding_id: old_binding.binding_id.to_string(),
            new_model_id: new_model.model_id.clone(),
            new_provider_id: new_model.provider_id.clone(),
            reason,
            context_fit,
            tool_schema_compatibility,
            parallel_tool_call_compatibility,
            reasoning_compatibility,
            modality_compatibility,
            compaction_backend_compatibility,
            provider_state_action,
            requires_compaction,
            lossy_mappings,
        }
    }

    fn check_context_fit(model: &ModelDescriptor, current_tokens: u64) -> CompatibilityVerdict {
        if current_tokens <= model.context_window {
            CompatibilityVerdict::Compatible
        } else if current_tokens <= model.context_window * 12 / 10 {
            // Within 20% — can compact to fit.
            CompatibilityVerdict::CompatibleWithLoss(vec![format!(
                "context window shrunk: {} → {} tokens (compaction needed)",
                current_tokens, model.context_window
            )])
        } else {
            CompatibilityVerdict::Incompatible(format!(
                "context too large: {} tokens > {} window",
                current_tokens, model.context_window
            ))
        }
    }

    fn check_tool_schema(_old: &ModelBinding, new: &ModelDescriptor) -> CompatibilityVerdict {
        if new.supports_tools {
            CompatibilityVerdict::Compatible
        } else {
            CompatibilityVerdict::Incompatible(
                "new model does not support tools".into(),
            )
        }
    }

    /// 3. Parallel tool call support: if the session uses parallel tool calls
    /// and the new model doesn't support them, the calls will be serialized
    /// (a performance loss, not a correctness loss).
    fn check_parallel_tool_calls(uses_parallel: bool, new: &ModelDescriptor) -> CompatibilityVerdict {
        if !uses_parallel || new.supports_parallel_tool_calls {
            CompatibilityVerdict::Compatible
        } else {
            CompatibilityVerdict::CompatibleWithLoss(vec![
                "parallel tool calls will be serialized (new model lacks support)".into()
            ])
        }
    }

    fn check_reasoning(old: &ModelBinding, new: &ModelDescriptor) -> CompatibilityVerdict {
        match (old.reasoning_policy, new.supports_reasoning) {
            (crate::binding::ReasoningPolicy::None, _) => CompatibilityVerdict::Compatible,
            (_, true) => CompatibilityVerdict::Compatible,
            (crate::binding::ReasoningPolicy::Visible, false) => {
                CompatibilityVerdict::CompatibleWithLoss(vec![
                    "visible reasoning will be lost".into()
                ])
            }
            (crate::binding::ReasoningPolicy::Hidden, false) => {
                CompatibilityVerdict::CompatibleWithLoss(vec![
                    "hidden reasoning envelope will be lost".into()
                ])
            }
        }
    }

    /// 5. Modality: if the current context contains images and the new model
    /// doesn't support image inputs, the switch is incompatible (images would
    /// be silently dropped).
    fn check_modality(has_images: bool, new: &ModelDescriptor) -> CompatibilityVerdict {
        if !has_images || new.supports_images {
            CompatibilityVerdict::Compatible
        } else {
            CompatibilityVerdict::Incompatible(
                "context contains images but new model does not support image inputs".into(),
            )
        }
    }

    /// 6. Compaction backend: if the session needs compaction (context near
    /// window limit) and the new model has no compaction capability, the
    /// switch is incompatible (compaction cannot proceed).
    fn check_compaction_backend(needs: bool, new: &ModelDescriptor) -> CompatibilityVerdict {
        if !needs || new.compaction_capabilities != crate::descriptor::CompactionCapabilities::None {
            CompatibilityVerdict::Compatible
        } else {
            CompatibilityVerdict::Incompatible(
                "session needs compaction but new model has no compaction backend".into(),
            )
        }
    }

    fn determine_provider_state(
        old: &ModelBinding,
        new: &ModelDescriptor,
    ) -> ProviderStateAction {
        if old.provider_id != new.provider_id {
            ProviderStateAction::ClearWithWarning
        } else {
            ProviderStateAction::Keep
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::WireProtocol;

    #[test]
    fn context_fit_compatible() {
        let model = ModelDescriptor {
            model_id: "test".into(), provider_id: "p".into(),
            wire_model_name: "test".into(), context_window: 100_000,
            max_output_tokens: 4096, tokenizer_id: None, tokenizer_version: None,
            supports_tools: true, supports_parallel_tool_calls: true,
            supports_reasoning: true, reasoning_modes: vec![],
            supports_images: false, supports_prompt_cache: false,
            supports_structured_output: false,
            compaction_capabilities: crate::descriptor::CompactionCapabilities::None,
            model_revision: 1,
        };
        assert!(CompatibilityGate::check_context_fit(&model, 50_000).is_fully_compatible());
    }

    #[test]
    fn context_too_large_for_window() {
        let model = ModelDescriptor {
            model_id: "small".into(), provider_id: "p".into(),
            wire_model_name: "small".into(), context_window: 10_000,
            max_output_tokens: 1024, tokenizer_id: None, tokenizer_version: None,
            supports_tools: false, supports_parallel_tool_calls: false,
            supports_reasoning: false, reasoning_modes: vec![],
            supports_images: false, supports_prompt_cache: false,
            supports_structured_output: false,
            compaction_capabilities: crate::descriptor::CompactionCapabilities::None,
            model_revision: 1,
        };
        let verdict = CompatibilityGate::check_context_fit(&model, 50_000);
        assert!(matches!(verdict, CompatibilityVerdict::Incompatible(_)));
    }

    #[test]
    fn no_tools_is_incompatible() {
        let model = ModelDescriptor {
            model_id: "notext".into(), provider_id: "p".into(),
            wire_model_name: "notext".into(), context_window: 100_000,
            max_output_tokens: 4096, tokenizer_id: None, tokenizer_version: None,
            supports_tools: false, supports_parallel_tool_calls: false,
            supports_reasoning: false, reasoning_modes: vec![],
            supports_images: false, supports_prompt_cache: false,
            supports_structured_output: false,
            compaction_capabilities: crate::descriptor::CompactionCapabilities::None,
            model_revision: 1,
        };
        let binding = ModelBinding::new("p".into(), 1, "old".into(), 1, WireProtocol::Responses);
        assert!(!CompatibilityGate::check_tool_schema(&binding, &model).is_compatible());
    }

    fn make_full_model() -> ModelDescriptor {
        ModelDescriptor {
            model_id: "full".into(), provider_id: "p".into(),
            wire_model_name: "full".into(), context_window: 100_000,
            max_output_tokens: 4096, tokenizer_id: None, tokenizer_version: None,
            supports_tools: true, supports_parallel_tool_calls: true,
            supports_reasoning: true, reasoning_modes: vec![],
            supports_images: true, supports_prompt_cache: false,
            supports_structured_output: false,
            compaction_capabilities: crate::descriptor::CompactionCapabilities::Local,
            model_revision: 1,
        }
    }

    #[test]
    fn parallel_tool_calls_loss_when_not_supported() {
        let mut model = make_full_model();
        model.supports_parallel_tool_calls = false;
        // Session uses parallel tool calls → loss
        let v = CompatibilityGate::check_parallel_tool_calls(true, &model);
        assert!(matches!(v, CompatibilityVerdict::CompatibleWithLoss(_)));
        // Session doesn't use parallel tool calls → fine
        let v = CompatibilityGate::check_parallel_tool_calls(false, &model);
        assert!(v.is_fully_compatible());
    }

    #[test]
    fn modality_incompatible_when_images_unsupported() {
        let mut model = make_full_model();
        model.supports_images = false;
        // Context has images → incompatible
        let v = CompatibilityGate::check_modality(true, &model);
        assert!(matches!(v, CompatibilityVerdict::Incompatible(_)));
        // No images → fine
        let v = CompatibilityGate::check_modality(false, &model);
        assert!(v.is_fully_compatible());
    }

    #[test]
    fn compaction_backend_incompatible_when_needed() {
        let mut model = make_full_model();
        model.compaction_capabilities = crate::descriptor::CompactionCapabilities::None;
        // Needs compaction → incompatible
        let v = CompatibilityGate::check_compaction_backend(true, &model);
        assert!(matches!(v, CompatibilityVerdict::Incompatible(_)));
        // Doesn't need → fine
        let v = CompatibilityGate::check_compaction_backend(false, &model);
        assert!(v.is_fully_compatible());
    }

    #[test]
    fn full_check_with_context_all_7_dimensions() {
        let old = ModelBinding::new("openai".into(), 1, "gpt-5".into(), 1, WireProtocol::Responses);
        let new_model = make_full_model();
        let ctx = SwitchContext {
            uses_parallel_tool_calls: true,
            has_images: true,
            needs_compaction_backend: true,
        };
        let plan = CompatibilityGate::check_with_context(&old, &new_model, 50_000, SwitchReason::UserRequested, &ctx);
        // All 7 should be compatible for a full-capability model.
        assert!(plan.context_fit.is_fully_compatible());
        assert!(plan.tool_schema_compatibility.is_fully_compatible());
        assert!(plan.parallel_tool_call_compatibility.is_fully_compatible());
        assert!(plan.reasoning_compatibility.is_fully_compatible());
        assert!(plan.modality_compatibility.is_fully_compatible());
        assert!(plan.compaction_backend_compatibility.is_fully_compatible());
        assert!(!plan.requires_compaction);
        assert!(plan.lossy_mappings.is_empty());
    }

    #[test]
    fn full_check_detects_multiple_losses() {
        let mut old = ModelBinding::new("openai".into(), 1, "gpt-5".into(), 1, WireProtocol::Responses);
        old.reasoning_policy = crate::binding::ReasoningPolicy::Visible;
        let mut new_model = make_full_model();
        new_model.supports_parallel_tool_calls = false;
        new_model.supports_reasoning = false;
        new_model.supports_images = false;
        let ctx = SwitchContext {
            uses_parallel_tool_calls: true,
            has_images: true,
            needs_compaction_backend: false,
        };
        let plan = CompatibilityGate::check_with_context(&old, &new_model, 50_000, SwitchReason::Failover, &ctx);
        // Parallel tool calls: loss (serialized)
        assert!(matches!(plan.parallel_tool_call_compatibility, CompatibilityVerdict::CompatibleWithLoss(_)));
        // Reasoning: loss (visible reasoning lost)
        assert!(matches!(plan.reasoning_compatibility, CompatibilityVerdict::CompatibleWithLoss(_)));
        // Modality: incompatible (images unsupported)
        assert!(matches!(plan.modality_compatibility, CompatibilityVerdict::Incompatible(_)));
        // Lossy mappings should include parallel + reasoning losses (not modality — that's incompatible)
        assert!(plan.lossy_mappings.len() >= 2);
    }
}
