//! Tool exposure classification.
//!
//! Controls how — and to whom — a capability is visible: the model,
//! nested code-mode agents, tool search, or only the app UI.

use serde::{Deserialize, Serialize};

/// Governs how a tool appears (or does not appear) to different consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolExposure {
    /// Full schema in the initial model request and in nested code mode.
    Direct,
    /// Discoverable via tool search but not in the initial context window.
    Deferred,
    /// Only callable from within a code-mode (sandboxed) sub-agent.
    CodeMode,
    /// Visible only to the app UI; never sent to the model.
    AppOnly,
    /// Registered in the runtime but invisible to all consumers.
    Internal,
    /// Explicitly disabled; treated as if the tool does not exist.
    Disabled,
}
