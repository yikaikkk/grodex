//! ApprovalBroker — centralized ticket management.
//!
//! Holds all pending approval tickets. Submits new tickets (returning
//! a oneshot receiver to the caller), resolves them when the frontend
//! sends a decision, and cancels all on session shutdown.
//!
//! Optionally backed by SQLite via `TicketStore` for crash recovery.
//! Fail-closed: if the DB is unavailable the broker degrades to pure
//! in-memory mode transparently.

use crate::store::{StoreError, TicketStore};
use crate::ticket::{ApprovalTicket, TicketStatus};
use crate::resolution::ApprovalResolution;
use grodex_core::policy::PolicyDecision;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Debug)]
pub struct ApprovalBroker {
    tickets: HashMap<String, ApprovalTicket>,
    default_timeout: Duration,
    store: Option<TicketStore>,
    #[allow(dead_code)]
    db_path: Option<PathBuf>,
}

impl ApprovalBroker {
    pub fn new(default_timeout: Duration) -> Self {
        Self {
            tickets: HashMap::new(),
            default_timeout,
            store: None,
            db_path: None,
        }
    }

    pub fn try_new_with_db<P: Into<PathBuf>>(
        default_timeout: Duration,
        db_path: P,
    ) -> Result<Self, StoreError> {
        let path = db_path.into();
        let store = TicketStore::new(&path)?;
        let mut broker = Self {
            tickets: HashMap::new(),
            default_timeout,
            store: Some(store),
            db_path: Some(path),
        };
        broker.recover_pending_from_store();
        Ok(broker)
    }

    pub fn with_db<P: Into<PathBuf>>(mut self, db_path: P) -> Result<Self, (Self, StoreError)> {
        let path = db_path.into();
        match TicketStore::new(&path) {
            Ok(store) => {
                self.store = Some(store);
                self.db_path = Some(path);
                self.recover_pending_from_store();
                Ok(self)
            }
            Err(e) => Err((self, e)),
        }
    }

    pub fn store(&self) -> Option<&TicketStore> {
        self.store.as_ref()
    }

    fn recover_pending_from_store(&mut self) {
        let Some(store) = &self.store else { return };
        let Ok(pending) = store.get_pending_tickets() else { return };
        for mut ticket in pending {
            let (tx, _rx) = oneshot::channel::<ApprovalResolution>();
            ticket.decision_tx = Some(tx);
            if ticket.summary.is_empty() {
                ticket.summary = "recovered-pending".to_string();
            } else if !ticket.summary.contains("recovered-pending") {
                ticket.summary = format!("{}\n[recovered-pending]", ticket.summary);
            }
            let id = ticket.ticket_id.clone();
            self.tickets.insert(id, ticket);
        }
    }

    pub fn submit(&mut self, ticket: ApprovalTicket) -> oneshot::Receiver<ApprovalResolution> {
        self.submit_ticket(ticket)
    }

    pub fn submit_ticket(&mut self, ticket: ApprovalTicket) -> oneshot::Receiver<ApprovalResolution> {
        let (tx, rx) = oneshot::channel();
        let ticket_id = ticket.ticket_id.clone();
        let ticket_with_tx = ApprovalTicket {
            ticket_id: ticket_id.clone(),
            tool_call_id: ticket.tool_call_id,
            tool_name: ticket.tool_name,
            summary: ticket.summary,
            risk_level: ticket.risk_level,
            status: TicketStatus::Pending,
            decision_tx: Some(tx),
            policy_decision: None,
            created_at: ticket.created_at,
            timeout: ticket.timeout,
            arguments_snapshot: ticket.arguments_snapshot,
            policy_rule_matches: ticket.policy_rule_matches,
            granted_by: None,
            session_id: ticket.session_id,
            source_agent_id: ticket.source_agent_id,
            task_id: ticket.task_id,
        };

        if let Some(store) = &self.store {
            let _ = store.upsert_ticket(&ticket_with_tx);
        }

        self.tickets.insert(ticket_id, ticket_with_tx);
        rx
    }

    pub fn resolve(
        &mut self,
        ticket_id: &str,
        decision: PolicyDecision,
        narrowed_args: Option<serde_json::Value>,
    ) -> bool {
        let Some(mut ticket) = self.tickets.remove(ticket_id) else {
            return false;
        };

        let status = match decision {
            PolicyDecision::Allow => TicketStatus::Approved,
            PolicyDecision::Deny | PolicyDecision::Ask => TicketStatus::Denied,
        };
        ticket.status = status;
        ticket.policy_decision = Some(decision);

        // P0-4 fix: narrowed arguments from the frontend must overwrite
        // the original arguments_snapshot in the ticket. A future
        // consumer that reads back this ticket (on resume, on lease
        // mint, etc.) will see the narrowed version, not the original
        // model-issued one.
        if let Some(narrowed) = narrowed_args {
            ticket.arguments_snapshot = Some(narrowed.clone());
        }

        if let Some(store) = &self.store {
            let granted_by: Option<&str> = None;
            let _ = store.update_status(ticket_id, status, Some(decision), granted_by);
            // Persist narrowed_args too if present, so resume doesn't
            // lose the narrow (the in-memory ticket is being dropped by
            // the remove() above).
            if let Some(args) = &ticket.arguments_snapshot {
                let _ = store.update_arguments_snapshot(ticket_id, args);
            }
        }

        // Build the ApprovalResolution to send through the channel.
        // This is where Narrow actually starts to work: the narrowed_args
        // are bundled into the resolution so the waiting tool future can
        // REPLACE its execution args with the narrowed subset.
        let resolution = match decision {
            PolicyDecision::Allow => {
                if let Some(narrowed) = &ticket.arguments_snapshot {
                    // The user approved with narrowed_args — construct
                    // a Narrow resolution that carries them.
                    ApprovalResolution::Narrow { narrowed_args: narrowed.clone() }
                } else {
                    ApprovalResolution::Allow
                }
            }
            PolicyDecision::Deny => ApprovalResolution::Deny,
            PolicyDecision::Ask => ApprovalResolution::Cancel,
        };

        if let Some(tx) = ticket.take_tx() {
            let _ = tx.send(resolution);
        }
        true
    }

    pub fn cancel_all(&mut self) {
        if let Some(store) = &self.store {
            let _ = store.cancel_all_pending();
        }
        for (_, mut ticket) in self.tickets.drain() {
            ticket.status = TicketStatus::Cancelled;
            ticket.policy_decision = Some(PolicyDecision::Deny);
            if let Some(tx) = ticket.take_tx() {
                let _ = tx.send(ApprovalResolution::Cancel);
            }
        }
    }

    pub fn expire_timed_out(&mut self) -> usize {
        let mut expired = Vec::new();
        for (id, ticket) in &self.tickets {
            if ticket.is_expired() {
                expired.push(id.clone());
            }
        }
        let count = expired.len();
        for id in expired {
            if let Some(mut ticket) = self.tickets.remove(&id) {
                ticket.status = TicketStatus::Expired;
                ticket.policy_decision = Some(PolicyDecision::Deny);
                if let Some(store) = &self.store {
                    let _ = store.update_status(
                        &id,
                        TicketStatus::Expired,
                        Some(PolicyDecision::Deny),
                        None,
                    );
                }
                if let Some(tx) = ticket.take_tx() {
                    let _ = tx.send(ApprovalResolution::Deny);
                }
            }
        }
        count
    }

    pub fn pending_count(&self) -> usize {
        self.tickets.len()
    }

    pub fn pending_tickets(&self) -> Vec<&str> {
        self.tickets.keys().map(|s| s.as_str()).collect()
    }

    /// Borrow a pending ticket by id (if still pending). Used by the
    /// supervisor just before resolve() to snapshot metadata for journal
    /// annotation.
    pub fn pending_ticket(&self, ticket_id: &str) -> Option<&ApprovalTicket> {
        self.tickets.get(ticket_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticket::RiskLevel;
    use grodex_core::id::ToolCallId;

    #[test]
    fn submit_and_resolve_ticket() {
        let mut broker = ApprovalBroker::new(Duration::from_secs(60));
        let (ticket, _rx) = ApprovalTicket::new(
            ToolCallId::new(),
            "read_file",
            "Read /tmp/test.txt",
            RiskLevel::Low,
        );

        let mut rx = broker.submit_ticket(ticket);
        assert_eq!(broker.pending_count(), 1);

        let ticket_id = broker.pending_tickets()[0].to_string();
        assert!(broker.resolve(&ticket_id, PolicyDecision::Allow, None));
        assert_eq!(broker.pending_count(), 0);

        let decision = rx.try_recv().unwrap();
        assert_eq!(decision, ApprovalResolution::Allow);
    }

    #[test]
    fn cancel_all_sends_deny() {
        let mut broker = ApprovalBroker::new(Duration::from_secs(60));
        let (ticket, _rx) = ApprovalTicket::new(
            ToolCallId::new(),
            "exec",
            "Run dangerous command",
            RiskLevel::High,
        );

        let mut rx = broker.submit_ticket(ticket);
        broker.cancel_all();

        let decision = rx.try_recv().unwrap();
        assert_eq!(decision, ApprovalResolution::Cancel);
    }

    #[test]
    fn resolve_unknown_ticket() {
        let mut broker = ApprovalBroker::new(Duration::from_secs(60));
        assert!(!broker.resolve("nonexistent", PolicyDecision::Allow, None));
    }

    #[test]
    fn in_memory_store_roundtrip() {
        let broker = ApprovalBroker::try_new_with_db(Duration::from_secs(60), ":memory:");
        assert!(broker.is_ok());
    }
}
