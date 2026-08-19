//! AgentNode — a stable, addressable agent identity in the parent-child tree.
//!
//! Design Doc 12, Section 7: AgentNode is the stable identity; TaskRun is one execution.
//! An AgentNode can execute multiple TaskRuns across follow-up interactions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for an agent node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse from a Uuid string. Returns None if the string is not a
    /// valid Uuid. Used by the durable sub-agent recovery path to
    /// rebuild the tree from journal event payloads.
    pub fn from_string(s: &str) -> Option<Self> {
        Uuid::parse_str(s).ok().map(Self)
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

/// Status of an agent node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    /// Agent is idle, waiting for a task.
    Idle,
    /// Agent is currently executing a task.
    Busy,
    /// Agent has completed all work and been unloaded.
    Completed,
    /// Agent encountered an unrecoverable error.
    Error,
}

/// A stable agent identity in the tree.
///
/// Holds metadata, status, and a command channel for sending tasks.
/// The node persists across multiple TaskRuns — it's the agent's "address."
#[derive(Debug, Clone)]
pub struct AgentNode {
    pub id: AgentId,
    /// Parent agent id (None for the root/main agent).
    pub parent_id: Option<AgentId>,
    /// Human-readable label.
    pub label: String,
    /// Current status.
    pub status: AgentStatus,
    /// When the node was created.
    pub created_at: DateTime<Utc>,
}

impl AgentNode {
    /// Create a new agent node.
    pub fn new(parent_id: Option<AgentId>, label: impl Into<String>) -> Self {
        Self {
            id: AgentId::new(),
            parent_id,
            label: label.into(),
            status: AgentStatus::Idle,
            created_at: Utc::now(),
        }
    }
}
