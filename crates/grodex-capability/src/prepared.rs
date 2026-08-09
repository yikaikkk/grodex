//! A fully prepared, validated, and policy-checked tool call ready for execution.
//!
//! This is the output of the Tool Pipeline (validation → policy → approval)
//! and the input to the Sandbox Executor or MCP Runtime.

use crate::id::CapabilityId;
use grodex_core::id::OperationId;
use grodex_core::id::StepSnapshotId;
use grodex_core::id::ToolCallId;
use grodex_core::policy::PolicyDecision;
use serde::{Deserialize, Serialize};

/// A capability call that has passed all pre-flight checks.
///
/// After this point, no capability definitions or policies can change
/// the execution semantics — the `capability_revision` and `policy_generation`
/// provide a complete audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedCapabilityCall {
    /// The original model-generated tool call id.
    pub tool_call_id: ToolCallId,
    /// Which snapshot governs this call's capability definitions.
    pub snapshot_id: StepSnapshotId,
    /// The resolved capability to invoke.
    pub capability_id: CapabilityId,
    /// The revision of the capability at preparation time.
    pub capability_revision: u64,
    /// Schema-validated arguments ready for execution.
    pub validated_args: serde_json::Value,
    /// Content hash of the validated arguments for auditing.
    pub args_hash: String,
    /// The strictest policy decision applied (ceiling — never escalated).
    pub policy_ceiling: PolicyDecision,
    /// The policy generation at preparation time.
    pub policy_generation: u64,
    /// Unique idempotency key for this operation.
    pub operation_id: OperationId,
}
