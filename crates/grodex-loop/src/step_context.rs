use crate::capability::CapabilitySnapshot;
use chrono::{DateTime, Utc};
use grodex_provider::canonical_request::ToolSpec;
use grodex_skills::SkillCatalog;
use std::sync::Arc;

use crate::turn::TurnContext;

#[derive(Debug, Clone)]
pub struct StepContext {
    pub turn: Arc<TurnContext>,
    pub tool_specs: Vec<ToolSpec>,
    pub skill_catalog: SkillCatalog,
    pub capability_snapshot: CapabilitySnapshot,
    pub captured_at: DateTime<Utc>,
}

impl StepContext {
    pub fn capture(
        turn: Arc<TurnContext>,
        tool_specs: Vec<ToolSpec>,
        skill_catalog: SkillCatalog,
        capability_snapshot: CapabilitySnapshot,
    ) -> Self {
        Self {
            turn,
            tool_specs,
            skill_catalog,
            capability_snapshot,
            captured_at: Utc::now(),
        }
    }
}
