//! ApprovalTicket — a oneshot channel-based request for user approval.
//!
//! Each tool call that requires approval gets a ticket. The caller
//! awaits the `decision_rx` end; the frontend sends a decision through
//! the broker. On timeout (default: fail closed → Deny).

use grodex_core::id::ToolCallId;
use grodex_core::policy::PolicyDecision;
use crate::resolution::ApprovalResolution;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Critical => "Critical",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Low" => Some(Self::Low),
            "Medium" => Some(Self::Medium),
            "High" => Some(Self::High),
            "Critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TicketStatus {
    Pending,
    Approved,
    Denied,
    Expired,
    Cancelled,
}

impl TicketStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Approved => "Granted",
            Self::Denied => "Denied",
            Self::Expired => "TimedOut",
            Self::Cancelled => "Cancelled",
        }
    }

    pub fn from_persistence_str(s: &str) -> Option<Self> {
        match s {
            "Pending" => Some(Self::Pending),
            "Granted" => Some(Self::Approved),
            "Denied" => Some(Self::Denied),
            "TimedOut" => Some(Self::Expired),
            "Cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct ApprovalTicket {
    pub ticket_id: String,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub summary: String,
    pub risk_level: RiskLevel,
    pub status: TicketStatus,
    pub decision_tx: Option<oneshot::Sender<ApprovalResolution>>,
    pub policy_decision: Option<PolicyDecision>,
    pub created_at: Instant,
    pub timeout: Duration,
    pub arguments_snapshot: Option<serde_json::Value>,
    pub policy_rule_matches: Option<serde_json::Value>,
    pub granted_by: Option<String>,
    pub session_id: Option<String>,
    pub source_agent_id: Option<String>,
    pub task_id: Option<String>,
}

impl ApprovalTicket {
    pub fn new(
        tool_call_id: ToolCallId,
        tool_name: impl Into<String>,
        summary: impl Into<String>,
        risk_level: RiskLevel,
    ) -> (Self, oneshot::Receiver<ApprovalResolution>) {
        let (tx, rx) = oneshot::channel();
        let ticket = Self {
            ticket_id: format!("ticket_{}", uuid::Uuid::new_v4()),
            tool_call_id,
            tool_name: tool_name.into(),
            summary: summary.into(),
            risk_level,
            status: TicketStatus::Pending,
            decision_tx: Some(tx),
            policy_decision: None,
            created_at: Instant::now(),
            timeout: Duration::from_secs(120),
            arguments_snapshot: None,
            policy_rule_matches: None,
            granted_by: None,
            session_id: None,
            source_agent_id: None,
            task_id: None,
        };
        (ticket, rx)
    }

    pub fn take_tx(&mut self) -> Option<oneshot::Sender<ApprovalResolution>> {
        self.decision_tx.take()
    }

    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.timeout
    }
}
