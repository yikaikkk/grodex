//! Grodex Prompt — system prompt assembly.
//!
//! Builds the system prompt from base instructions, skills, tools,
//! environment info, and project rules. Produces a versioned,
//! hashable `PromptManifest` for audit and cache invalidation.

pub mod builder;
pub mod conflict;
pub mod discovery;
pub mod instruction_event;
pub mod manifest;
pub mod slash;

pub use builder::{EnvironmentInfo, PromptBuilder};
pub use conflict::{detect_conflicts, ConflictKind, ConflictReport, InstructionConflict, MaskRecord};
pub use discovery::{DiscoveryConfig, DiscoveryResult, DiscoveryStats, InstructionDiscovery};
pub use instruction_event::{
    DrainedInstruction, InstructionDiscoveredEvent, InstructionEventKind,
    RuntimeInstructionInjector,
};
pub use manifest::{
    Authority, InstructionKind, InstructionNode, InstructionScope, NodeManifestEntry,
    PromptManifest, PromptSection, PromptZone, TrustState,
};
pub use slash::{
    SlashCommandError, SlashCommandKind, SlashCommandRegistry, SlashCommandSpec, SlashResolution,
};
