//! Grodex Capability — Tool, Skill, MCP, and Resource management model.
//!
//! This crate defines the data model for capabilities: how tools, skills,
//! MCP-provided operations, and app actions are identified, described,
//! exposed, and captured into immutable per-Step snapshots.
//!
//! Runtime enforcement lives in `manager` (Doc 10 §12–14):
//!   * `CapabilityManager::capture_turn_base` for turn-start capture
//!   * `suggest_overlay` / `apply_overlay` for per-Step deltas
//!   * `validate_turn_consistency` for end-of-turn invariant assertions

pub mod authority;
pub mod descriptor;
pub mod effective_revision;
pub mod explain;
pub mod exposure;
pub mod id;
pub mod manager;
pub mod prepared;
pub mod promotion;
pub mod router;
pub mod tool_search;

pub use effective_revision::{EffectiveToolCallRevision, TransformKind};
pub use explain::{
    BudgetStatus, CapabilityExplanation, CapabilityExplainer, CapabilityVisibilityFacts,
    CausalLink, GenerationStatus, McpConnStatus, PolicyVisibility, ProviderStatus,
    RevisionStatus, VisibilityStage, VisibilityVerdict,
};
pub use manager::{
    AppliedOverlay, Availability, CapabilityManager, CapabilityViolation, TurnBaseInputs, VResult,
};
pub use promotion::{DeferredPromotionDecision, DeferredPromotionRecord, DeferredPromotionRequest};
pub use router::{DefaultToolRouter, ToolRouter};
pub use tool_search::{
    CapabilityPromotedEvent, DeferredIndexEntry, DeferredToolIndex, IndexExclusion,
    IndexExclusionReason, PlannedPromotions, PromotionPlanner, SearchHit, SearchOutcome,
    StalePromotion,
};
