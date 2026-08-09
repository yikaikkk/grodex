//! Config generation — domain-scoped monotonic counters.
//!
//! Each domain (prompt, capability, policy, etc.) has its own generation
//! counter. Changes to UI-only settings don't invalidate the prompt cache.
//! Security tightening takes effect immediately via `policy` generation bump.

use crate::layer::ConfigLayer;
use crate::loader::ConfigPaths;
use crate::requirements::RequirementBinding;
use crate::values::EffectiveConfig;

/// Domain-scoped generation counters.
///
/// Each counter is a monotonic u64. A bump in one domain does not
/// affect others — UI-only changes don't bust the Tool/Prompt cache.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigGeneration {
    /// Root generation — bumped on any change.
    pub root: u64,
    /// Bumped when instructions, AGENTS.md, or prompt templates change.
    pub prompt: u64,
    /// Bumped when Tool, Skill, or MCP registrations change.
    pub capability: u64,
    /// Bumped when permission policies or allow/deny rules change.
    pub policy: u64,
    /// Bumped when sandbox profiles or paths change.
    pub sandbox: u64,
    /// Bumped when provider endpoints, model lists, or auth strategies change.
    pub provider: u64,
    /// Bumped when memory configuration changes.
    pub memory: u64,
    /// Bumped when UI settings change.
    pub ui: u64,
}

impl ConfigGeneration {
    /// Create a new generation starting at 1 for all domains.
    pub fn initial() -> Self {
        Self {
            root: 1,
            prompt: 1,
            capability: 1,
            policy: 1,
            sandbox: 1,
            provider: 1,
            memory: 1,
            ui: 1,
        }
    }

    /// Bump the root and all domain counters.
    pub fn bump_all(&mut self) {
        self.root += 1;
        self.prompt += 1;
        self.capability += 1;
        self.policy += 1;
        self.sandbox += 1;
        self.provider += 1;
        self.memory += 1;
        self.ui += 1;
    }

    /// Bump only the UI generation (for display-only changes).
    pub fn bump_ui(&mut self) {
        self.root += 1;
        self.ui += 1;
    }

    /// Bump policy generation (for security tightening).
    pub fn bump_policy(&mut self) {
        self.root += 1;
        self.policy += 1;
    }
}

/// The complete loaded configuration, ready for consumption by the runtime.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Merged effective values with origin traces.
    pub effective: EffectiveConfig,
    /// Non-overridable enterprise requirements.
    pub requirements: RequirementBinding,
    /// Domain-scoped generation counters.
    pub generation: ConfigGeneration,
    /// All raw layers before merging (for diagnostics and reload).
    pub raw_layers: Vec<ConfigLayer>,
    /// Paths that were loaded.
    pub paths: ConfigPaths,
}

impl LoadedConfig {
    /// Create an empty config with defaults (used when no config files exist).
    pub fn empty() -> Self {
        Self {
            effective: EffectiveConfig::empty(),
            requirements: RequirementBinding::default(),
            generation: ConfigGeneration::initial(),
            raw_layers: Vec::new(),
            paths: ConfigPaths::discover(&std::env::current_dir().unwrap_or_default()),
        }
    }
}
