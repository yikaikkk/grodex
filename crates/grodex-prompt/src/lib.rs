//! Grodex Prompt — system prompt assembly.
//!
//! Builds the system prompt from base instructions, skills, tools,
//! environment info, and project rules. Produces a versioned,
//! hashable `PromptManifest` for audit and cache invalidation.

pub mod builder;
pub mod discovery;
pub mod manifest;

pub use builder::{EnvironmentInfo, PromptBuilder};
pub use discovery::{DiscoveryConfig, DiscoveryResult, DiscoveryStats, InstructionDiscovery};
pub use manifest::{
    Authority, InstructionKind, InstructionNode, InstructionScope, NodeManifestEntry,
    PromptManifest, PromptSection, PromptZone, TrustState,
};
