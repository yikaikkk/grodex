//! Grodex V2 protocol extensions (namespace `x-agent/v2`).
//!
//! These types cover concepts that standard ACP cannot express: agent
//! tree management, durable tasks, structured approval, config diagnostics,
//! generation tracking, and replay cursors.

use serde::{Deserialize, Serialize};

/// The set of extensions the client supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionCapabilities {
    pub agent_tree: bool,
    pub durable_tasks: bool,
    pub approval_ticket: bool,
    pub config_diagnostics: bool,
    pub replay_cursor: bool,
}

/// An agent node in the parent-child tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTreeEvent {
    pub event_type: AgentTreeEventType,
    pub node_id: String,
    pub parent_id: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentTreeEventType {
    Spawned,
    Completed,
    Unloaded,
    Error,
}

/// Background / durable task lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableTaskEvent {
    pub task_id: String,
    pub status: DurableTaskStatus,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DurableTaskStatus {
    Created,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// A structured approval request sent to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalTicketV2 {
    pub ticket_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub summary: String,
    pub arguments_summary: String,
    pub risk_level: RiskLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

/// Configuration validation diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDiagnostic {
    pub level: DiagnosticLevel,
    pub key_path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

/// Cursor for session replay and resumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayCursor {
    pub session_id: String,
    pub last_seq: u64,
    pub generation: u64,
}
