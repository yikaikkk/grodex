//! Authority levels for capability providers.
//!
//! When two capabilities with the same canonical name conflict,
//! authority determines which one wins.

use serde::{Deserialize, Serialize};

/// The source of authority for a capability.
///
/// Higher-ranked authorities can override lower-ranked ones, but
/// managed/system authority is non-negotiable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Authority {
    /// Built into the agent binary itself (e.g. `read_file`, `exec`).
    Core = 0,
    /// Provided by the host application or IDE.
    Host = 10,
    /// Contributed by an installed plugin.
    Plugin = 20,
    /// An executor capability for running code in sandboxes.
    Executor = 30,
    /// Orchestrator-level capability (workflows, multi-agent coordination).
    Orchestrator = 40,
    /// Dynamically registered by an MCP server.
    Mcp = 50,
    /// App-only action that is never exposed to the model.
    App = 60,
}

impl Authority {
    /// Return a numeric authority level used for ceiling comparison.
    ///
    /// Values match the discriminants: a higher number means a broader
    /// source. An `authority_ceiling` of `N` means any authority with
    /// `level() > N` will be excluded from the promotable-id closure.
    pub fn level(self) -> u8 {
        // Safety: all discriminants are in the 0..=255 range. If a future
        // variant exceeds u8, this will panic at compile time via the
        // match-arm exhaustiveness (since we enumerate all variants here).
        match self {
            Authority::Core => 0,
            Authority::Host => 10,
            Authority::Plugin => 20,
            Authority::Executor => 30,
            Authority::Orchestrator => 40,
            Authority::Mcp => 50,
            Authority::App => 60,
        }
    }
}
