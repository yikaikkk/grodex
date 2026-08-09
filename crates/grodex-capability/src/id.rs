//! Capability identity — uniquely identifies one capability across all sources.

use crate::authority::Authority;
use serde::{Deserialize, Serialize};

/// The kind of capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CapabilityKind {
    /// A built-in or MCP-provided executable operation.
    Tool,
    /// A workflow / instruction set loaded from the filesystem.
    Skill,
    /// A passive resource (schema, document, template).
    Resource,
    /// An app-level action not exposed to the model.
    AppAction,
}

/// Globally unique identifier for one capability.
///
/// Composed of the provider's authority, provider-specific id, and kind.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CapabilityId {
    /// Who provides this capability.
    pub authority: Authority,
    /// Provider-scoped unique id (e.g. MCP server name).
    pub provider_id: String,
    /// What kind of capability this is.
    pub kind: CapabilityKind,
    /// Canonical name used for dispatch and deduplication.
    pub canonical_name: String,
}

impl CapabilityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityKind::Tool => "tool",
            CapabilityKind::Skill => "skill",
            CapabilityKind::Resource => "resource",
            CapabilityKind::AppAction => "app_action",
        }
    }
}

impl CapabilityId {
    pub fn new(
        authority: Authority,
        provider_id: impl Into<String>,
        kind: CapabilityKind,
        canonical_name: impl Into<String>,
    ) -> Self {
        Self {
            authority,
            provider_id: provider_id.into(),
            kind,
            canonical_name: canonical_name.into(),
        }
    }

    pub fn stable_hash_input(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.authority.level(),
            self.provider_id,
            self.kind.as_str(),
            self.canonical_name
        )
    }
}
