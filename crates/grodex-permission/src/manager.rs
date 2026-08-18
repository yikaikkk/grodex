//! PermissionManager — session-scoped coordinator.
//!
//! Combines the ApprovalBroker and PermissionPolicy into a single
//! entry point for the Tool Pipeline.

//! PermissionManager — session-scoped coordinator.
//!
//! Combines the ApprovalBroker and PermissionPolicy into a single
//! entry point for the Tool Pipeline. When `check()` returns `Ask`,
//! the manager optionally **publishes a notification** (see
//! `approval_tx`) so a frontend can surface the pending approval for
//! a user decision.

use crate::broker::ApprovalBroker;
use crate::policy::{ArgPattern, PermissionPolicy, PolicyRule};
use crate::ticket::{ApprovalTicket, RiskLevel};
use grodex_core::id::ToolCallId;
use grodex_core::policy::PolicyDecision;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Payload pushed through `PermissionManager.approval_tx` whenever a
/// new pending approval ticket is created (i.e. when policy says Ask).
///
/// Plain struct with no lifetimes so the permission crate can ship it
/// to higher layers without a circular dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequestedEvent {
    pub ticket_id: String,
    pub tool_name: String,
    pub summary: String,
    pub risk: String,
    /// Remaining timeout in milliseconds at creation time. The frontend
    /// counts down from here locally; the broker itself expires tickets
    /// at `ticket.timeout` anyway, so this is only for UX.
    pub timeout_remaining_ms: u64,
}

/// The result of a permission check — either immediate or requires approval.
#[derive(Debug)]
pub enum PermissionResult {
    /// Operation is allowed immediately (no user interaction).
    Allowed,
    /// Operation is denied immediately (by policy).
    Denied { reason: String },
    /// Operation requires user approval. Await the receiver for the decision.
    ApprovalRequired {
        ticket_id: String,
        decision_rx: oneshot::Receiver<PolicyDecision>,
    },
}

/// Session-scoped permission manager.
///
/// Coordinates policy evaluation and approval ticket management.
/// Created once per session and shared via Arc<Mutex<>> or channel.
pub struct PermissionManager {
    broker: ApprovalBroker,
    policy: PermissionPolicy,
    sandbox_validator: Option<SandboxValidator>,
    /// Monotonic revocation epoch — bumped when policies tighten.
    /// Previously-approved tickets with older epochs are invalidated.
    revocation_epoch: u64,
    /// Optional unbounded bus: the manager sends one
    /// `ApprovalRequestedEvent` through here every time `check()`
    /// creates a new pending ticket. Fail-silent if the channel is
    /// closed or full (frontend disconnected).
    approval_tx: Option<mpsc::UnboundedSender<ApprovalRequestedEvent>>,
}

/// Validates tool arguments against sandbox profiles.
pub struct SandboxValidator {
    /// Allowed read prefixes.
    read_prefixes: Vec<String>,
    /// Allowed write prefixes.
    write_prefixes: Vec<String>,
    /// Denied prefixes (takes precedence).
    deny_prefixes: Vec<String>,
}

impl SandboxValidator {
    pub fn from_profile(
        read_only: Vec<String>,
        read_write: Vec<String>,
        deny: Vec<String>,
    ) -> Self {
        Self {
            read_prefixes: read_only,
            write_prefixes: read_write,
            deny_prefixes: deny,
        }
    }

    fn is_denied(&self, path: &str) -> bool {
        self.deny_prefixes.iter().any(|d| path.starts_with(d))
    }

    fn can_read(&self, path: &str) -> bool {
        if self.is_denied(path) { return false; }
        self.read_prefixes.iter().any(|p| path.starts_with(p))
            || self.write_prefixes.iter().any(|p| path.starts_with(p))
    }

    fn can_write(&self, path: &str) -> bool {
        if self.is_denied(path) { return false; }
        self.write_prefixes.iter().any(|p| path.starts_with(p))
    }
}

impl PermissionManager {
    /// Create a new manager with the given policy (no approval bus).
    pub fn new(policy: PermissionPolicy) -> Self {
        Self {
            broker: ApprovalBroker::new(Duration::from_secs(120)),
            policy,
            sandbox_validator: None,
            revocation_epoch: 0,
            approval_tx: None,
        }
    }

    /// Attach an unbounded notification channel that fires exactly once
    /// per newly-created pending approval ticket. Used by turn
    /// coordinator so the session event bus learns about Ask decisions
    /// (→ the frontend renders a pending approval row).
    pub fn with_approval_bus(
        mut self,
        tx: mpsc::UnboundedSender<ApprovalRequestedEvent>,
    ) -> Self {
        self.approval_tx = Some(tx);
        self
    }

    /// Bump the revocation epoch. In-flight approvals from before this
    /// epoch are invalidated (must be re-approved).
    ///
    /// Invariant #16: revocation may only TIGHTEN. The epoch is monotonic
    /// and never decreases — a snapshot authorized at epoch N remains valid
    /// at epoch >= N only if the policy didn't tighten, and once tightened
    /// (epoch N+1) a prior approval is void.
    pub fn revoke_all(&mut self) {
        let prev = self.revocation_epoch;
        self.revocation_epoch = prev
            .checked_add(1)
            .expect("revocation epoch overflow — impossible in practice");
        debug_assert!(
            self.revocation_epoch > prev,
            "invariant #16: revocation epoch must strictly increase (monotonic tighten)"
        );
        self.broker.cancel_all();
    }

    /// Current revocation epoch.
    pub fn revocation_epoch(&self) -> u64 {
        self.revocation_epoch
    }

    /// Attach a sandbox validator for path-based policy enforcement.
    pub fn with_sandbox(mut self, validator: SandboxValidator) -> Self {
        self.sandbox_validator = Some(validator);
        self
    }

    /// Load policy rules from a TOML config value.
    pub fn load_policy_from_config(config: &toml::Value) -> PermissionPolicy {
        let mut policy = PermissionPolicy::new();
        if let Some(rules) = config.get("rules").and_then(|v| v.as_array()) {
            for rule in rules {
                let tool_pattern = rule.get("tool").and_then(|v| v.as_str()).unwrap_or("*");
                let decision = match rule.get("decision").and_then(|v| v.as_str()) {
                    Some("allow") => PolicyDecision::Allow,
                    Some("deny") => PolicyDecision::Deny,
                    _ => PolicyDecision::Ask,
                };
                let priority = rule.get("priority").and_then(|v| v.as_integer()).unwrap_or(0) as u8;
                let mut arg_patterns = Vec::new();
                if let Some(args) = rule.get("args").and_then(|v| v.as_array()) {
                    for arg in args {
                        if let (Some(path), Some(pattern)) = (
                            arg.get("path").and_then(|v| v.as_str()),
                            arg.get("pattern").and_then(|v| v.as_str()),
                        ) {
                            arg_patterns.push(ArgPattern {
                                arg_path: path.to_string(),
                                pattern: pattern.to_string(),
                            });
                        }
                    }
                }
                policy.add_rule(PolicyRule {
                    tool_pattern: tool_pattern.to_string(),
                    arg_patterns,
                    command: None,
                    resource: None,
                    rule_id: None,
                    network: None,
                    mcp: None,
                    decision,
                    priority,
                });
            }
        }
        policy
    }

    /// Check sandbox compliance for path-based operations.
    /// Returns false if the operation is blocked by the sandbox.
    pub fn check_sandbox(&self, tool_name: &str, args: &serde_json::Value) -> Result<(), String> {
        let validator = match &self.sandbox_validator {
            Some(v) => v,
            None => return Ok(()), // no sandbox = permissive
        };

        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");

        match tool_name {
            "read_file" if !validator.can_read(path) => {
                return Err(format!("sandbox: read denied for {path}"));
            }
            "write_file" | "edit_file" if !validator.can_write(path) => {
                return Err(format!("sandbox: write denied for {path}"));
            }
            "read_file" | "write_file" | "edit_file" => {} // allowed
            _ => {} // non-file tools pass through
        }

        Ok(())
    }

    /// Check whether a tool call is permitted.
    ///
    /// Returns `Allowed` if policy says Allow, `Denied` if policy says Deny,
    /// or `ApprovalRequired` with a oneshot receiver if policy says Ask.
    pub fn check(
        &mut self,
        tool_call_id: ToolCallId,
        tool_name: &str,
        args: &serde_json::Value,
        summary: &str,
    ) -> PermissionResult {
        let decision = self.policy.evaluate(tool_name, args);

        match decision {
            PolicyDecision::Allow => PermissionResult::Allowed,
            PolicyDecision::Deny => PermissionResult::Denied {
                reason: format!("policy denied {tool_name}"),
            },
            PolicyDecision::Ask => {
                let risk = Self::assess_risk(tool_name, args);
                let (ticket, _rx_from_ticket) = ApprovalTicket::new(tool_call_id, tool_name, summary, risk);
                let ticket_id = ticket.ticket_id.clone();
                let tool_name_owned = ticket.tool_name.clone();
                let summary_owned = ticket.summary.clone();
                let risk_str = format!("{:?}", ticket.risk_level);
                let timeout_ms = ticket
                    .timeout
                    .as_millis()
                    .min(u64::MAX as u128) as u64;
                // ── submit_ticket creates the broker-owned oneshot channel
                // that resolve() will actually send through. The receiver
                // from ApprovalTicket::new is a DIFFERENT channel that no
                // one will ever send to — using it was the root cause of
                // the "approval result never reaches the waiting tool"
                // main-chain break (the tool hung on a dead receiver).
                let decision_rx = self.broker.submit_ticket(ticket);
                // ── Fire the approval bus so the frontend learns a ticket
                // is pending (Design Doc 16 §10, first half of the round-
                // trip). Fail-silent: if the bus is closed/disconnected
                // (no frontend, or shutdown) the ticket still lives in the
                // broker and expires after its timeout — the tool future
                // remains correct, just unnotified.
                if let Some(tx) = self.approval_tx.as_ref() {
                    let _ = tx.send(ApprovalRequestedEvent {
                        ticket_id: ticket_id.clone(),
                        tool_name: tool_name_owned,
                        summary: summary_owned,
                        risk: risk_str,
                        timeout_remaining_ms: timeout_ms,
                    });
                }
                PermissionResult::ApprovalRequired {
                    ticket_id,
                    decision_rx,
                }
            }
        }
    }

    /// Resolve a pending approval ticket.
    pub fn resolve(&mut self, ticket_id: &str, decision: PolicyDecision) -> bool {
        self.broker.resolve(ticket_id, decision)
    }

    /// Cancel all pending approvals (session shutdown).
    pub fn cancel_all(&mut self) {
        self.broker.cancel_all();
    }

    /// Expire timed-out tickets.
    pub fn expire_timed_out(&mut self) -> usize {
        self.broker.expire_timed_out()
    }

    /// Number of pending approval tickets.
    pub fn pending_count(&self) -> usize {
        self.broker.pending_count()
    }

    /// Assess risk level based on tool name and arguments.
    fn assess_risk(tool_name: &str, args: &serde_json::Value) -> RiskLevel {
        match tool_name {
            "read_file" => RiskLevel::Low,
            "exec" | "bash" => {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if cmd.contains("sudo") || cmd.contains("rm -rf") {
                    RiskLevel::Critical
                } else {
                    RiskLevel::Medium
                }
            }
            "write" | "edit" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.starts_with("/etc") || path.starts_with("/System") {
                    RiskLevel::High
                } else {
                    RiskLevel::Low
                }
            }
            _ => RiskLevel::Medium,
        }
    }
}

impl Default for PermissionManager {
    fn default() -> Self {
        Self::new(PermissionPolicy::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissive_policy_allows_immediately() {
        let mut mgr = PermissionManager::new(PermissionPolicy::permissive());
        let result = mgr.check(
            ToolCallId::new(),
            "read_file",
            &serde_json::json!({"path": "/tmp/test.txt"}),
            "Read a file",
        );
        assert!(matches!(result, PermissionResult::Allowed));
    }

    #[test]
    fn deny_policy_blocks() {
        let mut mgr = PermissionManager::new(PermissionPolicy::default_deny());
        let result = mgr.check(
            ToolCallId::new(),
            "exec",
            &serde_json::json!({"command": "ls"}),
            "Run ls",
        );
        assert!(matches!(result, PermissionResult::Denied { .. }));
    }

    #[test]
    fn ask_policy_creates_ticket() {
        let mut mgr = PermissionManager::new(PermissionPolicy::new());
        let result = mgr.check(
            ToolCallId::new(),
            "write",
            &serde_json::json!({"path": "/tmp/out.txt"}),
            "Write a file",
        );
        match result {
            PermissionResult::ApprovalRequired {
                ticket_id,
                decision_rx: _rx,
            } => {
                assert!(!ticket_id.is_empty());
                assert!(mgr.resolve(&ticket_id, PolicyDecision::Allow));
            }
            other => panic!("expected ApprovalRequired, got {other:?}"),
        }
    }

    /// End-to-end approval main chain (Design Doc 16 §10):
    ///
    ///   config rules → policy → check() Ask → ApprovalRequestedEvent
    ///     → resolve(Allow) → decision_rx wakes the waiting tool.
    ///
    /// This is the single most direct "main chain" test: it asserts that
    /// (a) the approval bus fires when policy says Ask, (b) the ticket id
    /// in the event matches the one returned from `check`, and (c) calling
    /// `resolve` actually delivers the decision to the oneshot receiver
    /// the tool is awaiting on. If any of these three links is broken,
    /// side-effecting tools hang or run unapproved.
    #[tokio::test]
    async fn end_to_end_approval_chain_fires_bus_and_wakes_tool() {
        // 1. Build a policy that mimics `build_permission_policy` output:
        //    read_file=allow, everything else=ask.
        let mut policy = PermissionPolicy::new();
        policy.add_rule(PolicyRule {
            tool_pattern: "read_file".into(),
            arg_patterns: Vec::new(),
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Allow,
            priority: 0,
        });
        policy.add_rule(PolicyRule {
            tool_pattern: "*".into(),
            arg_patterns: Vec::new(),
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Ask,
            priority: 0,
        });

        // 2. Wire the approval bus (mirrors TurnCoordinator::wire_approval_bus).
        let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
        let mut mgr = PermissionManager::new(policy).with_approval_bus(approval_tx);

        // 3. check() a side-effecting tool → Ask branch must fire the bus.
        let result = mgr.check(
            ToolCallId::new(),
            "exec",
            &serde_json::json!({"command": "ls"}),
            "Run ls",
        );
        let PermissionResult::ApprovalRequired {
            ticket_id,
            decision_rx,
        } = result
        else {
            panic!("expected ApprovalRequired for exec under Ask policy");
        };

        // 4. The bus must have delivered an ApprovalRequestedEvent whose
        //    ticket_id matches the one handed back from check().
        let ev = approval_rx
            .recv()
            .await
            .expect("ApprovalRequestedEvent must be fired on Ask");
        assert_eq!(ev.ticket_id, ticket_id);
        assert_eq!(ev.tool_name, "exec");
        assert!(ev.timeout_remaining_ms > 0);

        // 5. resolve(Allow) must return true and wake the waiting tool.
        assert!(mgr.resolve(&ticket_id, PolicyDecision::Allow));
        let decision = decision_rx
            .await
            .expect("decision_rx must be woken after resolve()");
        assert_eq!(decision, PolicyDecision::Allow);
    }

    /// End-to-end Deny path: resolve(Deny) must wake the tool with Deny so
    /// the tool pipeline aborts instead of hanging on a dead receiver.
    #[tokio::test]
    async fn end_to_end_approval_deny_wakes_tool_with_deny() {
        let mut policy = PermissionPolicy::new();
        policy.add_rule(PolicyRule {
            tool_pattern: "*".into(),
            arg_patterns: Vec::new(),
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Ask,
            priority: 0,
        });

        let (approval_tx, _approval_rx) = mpsc::unbounded_channel();
        let mut mgr = PermissionManager::new(policy).with_approval_bus(approval_tx);

        let result = mgr.check(
            ToolCallId::new(),
            "write_file",
            &serde_json::json!({"path": "/tmp/x"}),
            "Write file",
        );
        let PermissionResult::ApprovalRequired {
            ticket_id,
            decision_rx,
        } = result
        else {
            panic!("expected ApprovalRequired");
        };

        assert!(mgr.resolve(&ticket_id, PolicyDecision::Deny));
        let decision = decision_rx
            .await
            .expect("decision_rx must be woken after Deny resolve");
        assert_eq!(decision, PolicyDecision::Deny);
    }
}
