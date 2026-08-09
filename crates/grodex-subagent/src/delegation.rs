//! DelegationEnvelope — the frozen security/authority envelope a parent
//! hands to a child agent at spawn time.
//!
//! Audit (Phase 5-2): the audit's table flagged "DelegationEnvelope 全缺" and
//! "child 权限 ≤ parent" (invariant #12) as unimplemented — a sub-agent had
//! no security boundary and could in principle inherit the parent's full
//! authority. This module defines the envelope and the ceiling-check helpers
//! that make invariant #12 enforceable.
//!
//! The envelope carries the parent's authority ceiling, the capability subset
//! the child may use, the policy ceiling (strictest decision), the sandbox
//! profile, and a resource budget. After spawn nothing in the envelope can
//! change — the child executes against exactly this grant, and any attempt by
//! the child to exceed it (call a tool not in the subset, need a stricter
//! policy than the ceiling allows, write outside the sandbox) is rejected
//! before side-effect.

use grodex_core::id::ToolCallId;
use grodex_core::policy::PolicyDecision;
use grodex_sandbox_types::profile::SandboxProfile;
use serde::{Deserialize, Serialize};

/// The capability-name subset a child is permitted to invoke. Any tool not in
/// this set is rejected at dispatch (invariant #12: child authority ≤ parent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySubset {
    /// Allowed tool names (e.g. `["read_file", "exec"]`). An empty set means
    /// "no tools" (a pure-reasoning sub-agent).
    pub allowed_tools: Vec<String>,
    /// Allowed skill names, if any.
    #[serde(default)]
    pub allowed_skills: Vec<String>,
    /// Allowed MCP servers, if any.
    #[serde(default)]
    pub allowed_mcp_servers: Vec<String>,
}

impl CapabilitySubset {
    /// True if `tool_name` is in the allowed set.
    pub fn permits_tool(&self, tool_name: &str) -> bool {
        self.allowed_tools.iter().any(|t| t == tool_name)
    }
}

/// Resource budget for a delegated task (turns + wall-clock + tokens).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DelegationBudget {
    pub max_turns: Option<u32>,
    pub max_duration_secs: Option<u64>,
    pub max_total_tokens: Option<u64>,
}

/// The frozen delegation envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationEnvelope {
    /// The parent agent id that issued this envelope.
    pub parent_agent_id: String,
    /// The child agent id this envelope binds.
    pub child_agent_id: String,
    /// Capability subset the child may invoke.
    pub capabilities: CapabilitySubset,
    /// Strictest policy decision the child may self-grant. The child's own
    /// PermissionManager may produce anything UP TO this ceiling; it may not
    /// exceed it (a parent that delegated `Ask` cannot let the child `Allow`).
    pub policy_ceiling: PolicyDecision,
    /// Sandbox profile enforced for every side-effect the child performs.
    pub sandbox: SandboxProfile,
    /// Resource budget.
    pub budget: DelegationBudget,
    /// Authority ceiling (0–255, mirroring `Authority` discriminant). The
    /// child MUST NOT invoke capabilities whose authority exceeds this.
    pub authority_ceiling: u8,
    /// Revocation epoch pinned at delegation time. A parent that later
    /// revokes bumps its epoch; the child's envelope is then stale and any
    /// further side-effect is rejected (revalidation, invariant #16).
    pub revocation_epoch: u64,
    /// Monotonic delegation generation — bumped whenever the parent re-delegates.
    pub delegation_generation: u64,
}

/// Errors from delegation enforcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationError {
    /// The tool is not in the delegated capability subset.
    ToolNotDelegated(String),
    /// The requested policy decision exceeds the delegated ceiling.
    PolicyCeilingExceeded,
    /// The requested operation's authority exceeds the delegated ceiling.
    AuthorityCeilingExceeded { requested: u8, ceiling: u8 },
    /// The parent's revocation epoch advanced past the envelope's epoch.
    Revoked { envelope_epoch: u64, live_epoch: u64 },
}

impl std::fmt::Display for DelegationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolNotDelegated(t) => write!(f, "tool {t:?} not in delegated capability subset"),
            Self::PolicyCeilingExceeded => write!(f, "requested policy exceeds the delegated ceiling"),
            Self::AuthorityCeilingExceeded { requested, ceiling } => {
                write!(f, "authority {requested} exceeds delegated ceiling {ceiling}")
            }
            Self::Revoked { envelope_epoch, live_epoch } => {
                write!(f, "delegation revoked (envelope epoch {envelope_epoch} < live {live_epoch})")
            }
        }
    }
}

impl std::error::Error for DelegationError {}

impl DelegationEnvelope {
    /// Authorize a tool call against the envelope. Returns Ok(()) if the call
    /// is within the delegated bounds, Err otherwise. This is the invariant
    /// #12 enforcement point.
    pub fn authorize_tool_call(
        &self,
        tool_name: &str,
        tool_authority: u8,
        requested_policy: PolicyDecision,
        live_revocation_epoch: u64,
    ) -> Result<(), DelegationError> {
        // 1. Revocation: parent's live epoch must not have advanced.
        if live_revocation_epoch > self.revocation_epoch {
            return Err(DelegationError::Revoked {
                envelope_epoch: self.revocation_epoch,
                live_epoch: live_revocation_epoch,
            });
        }
        // 2. Tool subset.
        if !self.capabilities.permits_tool(tool_name) {
            return Err(DelegationError::ToolNotDelegated(tool_name.to_string()));
        }
        // 3. Authority ceiling.
        if tool_authority > self.authority_ceiling {
            return Err(DelegationError::AuthorityCeilingExceeded {
                requested: tool_authority,
                ceiling: self.authority_ceiling,
            });
        }
        // 4. Policy ceiling: the child's requested decision may not be MORE
        //    permissive than the ceiling. Allow=0, Ask=1, Deny=2 (strictness);
        //    "more permissive" = lower strictness. So requested.strictness()
        //    must be >= ceiling.strictness() (at least as strict).
        if strictness(requested_policy) < strictness(self.policy_ceiling) {
            return Err(DelegationError::PolicyCeilingExceeded);
        }
        Ok(())
    }
}

/// Strictness ranking: Allow=0, Ask=1, Deny=2 (mirrors permission::policy).
fn strictness(d: PolicyDecision) -> u8 {
    match d {
        PolicyDecision::Allow => 0,
        PolicyDecision::Ask => 1,
        PolicyDecision::Deny => 2,
    }
}

/// Builder for a DelegationEnvelope so a parent constructs one without a
/// 10-argument constructor.
#[derive(Debug, Clone)]
pub struct DelegationEnvelopeBuilder {
    parent_agent_id: String,
    child_agent_id: String,
    capabilities: CapabilitySubset,
    policy_ceiling: PolicyDecision,
    sandbox: SandboxProfile,
    budget: DelegationBudget,
    authority_ceiling: u8,
    revocation_epoch: u64,
    delegation_generation: u64,
}

impl DelegationEnvelopeBuilder {
    pub fn new(parent_agent_id: impl Into<String>, child_agent_id: impl Into<String>) -> Self {
        Self {
            parent_agent_id: parent_agent_id.into(),
            child_agent_id: child_agent_id.into(),
            capabilities: CapabilitySubset {
                allowed_tools: vec![],
                allowed_skills: vec![],
                allowed_mcp_servers: vec![],
            },
            policy_ceiling: PolicyDecision::Ask,
            sandbox: SandboxProfile {
                name: "restricted".into(),
                read_only_paths: vec![],
                read_write_paths: vec![],
                deny_paths: vec!["/".into()],
                network_rules: vec![],
                allow_exec: false,
                allow_fork: false,
            },
            budget: DelegationBudget::default(),
            authority_ceiling: 0,
            revocation_epoch: 1,
            delegation_generation: 1,
        }
    }

    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.capabilities.allowed_tools = tools;
        self
    }
    pub fn with_policy_ceiling(mut self, ceiling: PolicyDecision) -> Self {
        self.policy_ceiling = ceiling;
        self
    }
    pub fn with_sandbox(mut self, sandbox: SandboxProfile) -> Self {
        self.sandbox = sandbox;
        self
    }
    pub fn with_budget(mut self, budget: DelegationBudget) -> Self {
        self.budget = budget;
        self
    }
    pub fn with_authority_ceiling(mut self, ceiling: u8) -> Self {
        self.authority_ceiling = ceiling;
        self
    }
    pub fn with_revocation_epoch(mut self, epoch: u64) -> Self {
        self.revocation_epoch = epoch;
        self
    }
    pub fn with_delegation_generation(mut self, cap_gen: u64) -> Self {
        self.delegation_generation = cap_gen;
        self
    }

    pub fn build(self) -> DelegationEnvelope {
        DelegationEnvelope {
            parent_agent_id: self.parent_agent_id,
            child_agent_id: self.child_agent_id,
            capabilities: self.capabilities,
            policy_ceiling: self.policy_ceiling,
            sandbox: self.sandbox,
            budget: self.budget,
            authority_ceiling: self.authority_ceiling,
            revocation_epoch: self.revocation_epoch,
            delegation_generation: self.delegation_generation,
        }
    }
}

// Keep the ToolCallId import honest — the envelope is keyed per-tool-call at
// enforcement time in the live loop, even though `authorize_tool_call` takes
// the tool name by &str for testability.
#[allow(dead_code)]
fn _anchor_import(_: ToolCallId) {}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_sandbox_types::profile::NetworkRule;

    fn envelope() -> DelegationEnvelope {
        DelegationEnvelopeBuilder::new("parent", "child")
            .with_tools(vec!["read_file".into()])
            .with_policy_ceiling(PolicyDecision::Allow)
            .with_authority_ceiling(10)
            .with_revocation_epoch(3)
            .build()
    }

    #[test]
    fn allows_delegated_tool_within_bounds() {
        let env = envelope();
        assert!(env
            .authorize_tool_call("read_file", 0, PolicyDecision::Allow, 3)
            .is_ok());
    }

    #[test]
    fn rejects_tool_not_in_subset() {
        let env = envelope();
        let err = env
            .authorize_tool_call("exec", 0, PolicyDecision::Allow, 3)
            .unwrap_err();
        assert!(matches!(err, DelegationError::ToolNotDelegated(_)));
    }

    #[test]
    fn rejects_more_permissive_policy_than_ceiling() {
        // Ceiling = Deny (strictest=2). The child requesting Allow
        // (strictest=0) is MORE permissive than the ceiling ⇒ rejected.
        // (A child may always be stricter than the ceiling, never looser.)
        let env = DelegationEnvelopeBuilder::new("p", "c")
            .with_tools(vec!["read_file".into()])
            .with_policy_ceiling(PolicyDecision::Deny)
            .build();
        let err = env
            .authorize_tool_call("read_file", 0, PolicyDecision::Allow, 1)
            .unwrap_err();
        assert_eq!(err, DelegationError::PolicyCeilingExceeded);
    }

    #[test]
    fn allows_child_to_be_stricter_than_ceiling() {
        // Ceiling = Allow; child requests Deny (stricter) ⇒ allowed.
        let env = envelope(); // ceiling Allow
        assert!(env
            .authorize_tool_call("read_file", 0, PolicyDecision::Deny, 3)
            .is_ok());
    }

    #[test]
    fn rejects_authority_above_ceiling() {
        let env = envelope(); // ceiling 10
        let err = env
            .authorize_tool_call("read_file", 50, PolicyDecision::Allow, 3)
            .unwrap_err();
        assert!(matches!(
            err,
            DelegationError::AuthorityCeilingExceeded { requested: 50, ceiling: 10 }
        ));
    }

    #[test]
    fn rejects_when_parent_revocation_advanced() {
        let env = envelope(); // epoch 3
        let err = env
            .authorize_tool_call("read_file", 0, PolicyDecision::Allow, 4)
            .unwrap_err();
        assert!(matches!(err, DelegationError::Revoked { envelope_epoch: 3, live_epoch: 4 }));
    }

    /// Sanity-check the sandbox binding carried by the envelope survives a
    /// round-trip and is non-trivial.
    #[test]
    fn envelope_carries_sandbox_profile() {
        let sb = SandboxProfile {
            name: "workspace".into(),
            read_only_paths: vec!["/".into()],
            read_write_paths: vec![".".into()],
            deny_paths: vec!["/etc".into()],
            network_rules: vec![NetworkRule::AllowLocal],
            allow_exec: true,
            allow_fork: false,
        };
        let env = DelegationEnvelopeBuilder::new("p", "c")
            .with_sandbox(sb.clone())
            .build();
        assert_eq!(env.sandbox.name, "workspace");
        assert!(env.sandbox.allow_exec);
    }
}
