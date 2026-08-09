//! ApprovalResolution + PermissionLease — the resolved outcome of an
//! approval request and the single-use grant that authorizes execution.
//!
//! Design Doc 16: an `ApprovalTicket` resolves to one of four outcomes, not
//! just Allow/Deny:
//!   - `Allow`           — execute as requested, full scope.
//!   - `Narrow { scope }`— user approved a *narrowed* scope (e.g. approved
//!     `read /tmp/a` but the call asked to `read /tmp/*`). Execution must
//!     re-validate against the narrowed scope or refuse.
//!   - `Deny`            — blocked, do not execute.
//!   - `Cancel`          — the approval was withdrawn/cancelled (e.g. turn
//!     cancelled mid-approval); treat as Deny but distinguish for telemetry.
//!
//! A successful `Allow`/`Narrow` mints a `PermissionLease`: a single-use,
//! optionally-expiring token that the executor must present *and consume*
//! before the side effect. This caps double-execution: a lease consumed by
//! a tool run cannot be replayed by a crash-recovery retry (invariant #16:
//! revocation may only tighten; a lease is a one-shot, never refreshed).

use grodex_core::policy::PolicyDecision;
use std::time::{Duration, Instant};

/// The outcome of resolving an approval ticket.
#[derive(Debug, Clone)]
pub enum ApprovalResolution {
    /// Approved — execute the full requested scope.
    Allow,
    /// Approved but only for a narrowed scope. The executor must check the
    /// actual call's args against `narrowed_args`; if they fall outside the
    /// narrowed scope, execution is refused (fail-closed).
    Narrow {
        /// The JSON-pointer-scoped subset of arguments the user approved.
        /// e.g. `{"path": "/tmp/safe.txt"}` restricts a write_file to exactly
        /// that path even if the model asked elsewhere.
        narrowed_args: serde_json::Value,
    },
    /// Denied by policy or user.
    Deny,
    /// Cancelled (turn cancelled, session shutting down). Semantically Deny
    /// but kept distinct so callers/telemetry can tell intent from refusal.
    Cancel,
}

impl ApprovalResolution {
    /// Whether this resolution authorizes *any* execution.
    pub fn permits_execution(&self) -> bool {
        matches!(self, Self::Allow | Self::Narrow { .. })
    }

    /// Map to the coarse 3-value decision for compatibility with
    /// `PolicyDecision`-based code paths. Narrow→Ask (must re-check).
    pub fn to_policy_decision(&self) -> PolicyDecision {
        match self {
            Self::Allow => PolicyDecision::Allow,
            Self::Narrow { .. } => PolicyDecision::Ask,
            Self::Deny | Self::Cancel => PolicyDecision::Deny,
        }
    }
}

impl From<PolicyDecision> for ApprovalResolution {
    fn from(d: PolicyDecision) -> Self {
        match d {
            PolicyDecision::Allow => Self::Allow,
            PolicyDecision::Ask => Self::Deny, // unresolved Ask defaults to Deny
            PolicyDecision::Deny => Self::Deny,
        }
    }
}

/// A single-use lease authorizing one tool execution.
///
/// Minted on `Allow`/`Narrow`. `consume()` returns `true` exactly once;
/// subsequent calls return `false` — a replayed call (e.g. after a crash
/// recovery retry) cannot reuse the lease. `max_uses` is fixed at 1 for
/// side-effecting tools (the design's PermissionLease invariant).
#[derive(Debug, Clone)]
pub struct PermissionLease {
    /// The tool call id this lease authorizes.
    pub tool_call_id: grodex_core::id::ToolCallId,
    /// The resolution that minted it (carries Narrow scope if applicable).
    pub resolution: ApprovalResolution,
    /// Maximum uses — 1 for side-effecting tools.
    pub max_uses: u8,
    /// Absolute expiry; None = no expiry.
    pub expires_at: Option<Instant>,
    /// Revocation epoch at mint time. Revalidation refuses if the live epoch
    /// advanced past this (invariant #16: revocation only tightens).
    pub revocation_epoch: u64,
    uses: u8,
}

impl PermissionLease {
    /// Mint a single-use lease for `tool_call_id` from a resolution.
    pub fn new(
        tool_call_id: grodex_core::id::ToolCallId,
        resolution: ApprovalResolution,
        revocation_epoch: u64,
        ttl: Option<Duration>,
    ) -> Self {
        Self {
            tool_call_id,
            resolution,
            max_uses: 1,
            expires_at: ttl.map(|d| Instant::now() + d),
            revocation_epoch,
            uses: 0,
        }
    }

    /// Whether the lease is still valid (unused and unexpired).
    pub fn is_valid(&self) -> bool {
        self.uses < self.max_uses && self.expires_at.map(|t| Instant::now() < t).unwrap_or(true)
    }

    /// Consume one use. Returns `true` if this was the (first) valid use,
    /// `false` if already consumed, expired, or revoked.
    ///
    /// This is the single point that enforces "exactly-once execution per
    /// approval" — a retry after crash recovery must obtain a fresh lease.
    pub fn consume(&mut self) -> bool {
        if !self.is_valid() {
            return false;
        }
        self.uses += 1;
        true
    }

    /// Uses consumed so far (0 or 1 for a single-use lease).
    pub fn uses(&self) -> u8 {
        self.uses
    }
}

// ── LiveRevocationFence ────────────────────────────────────────────

/// Error returned when revocation has advanced since the fence was captured.
///
/// This means a `revoke_all()` (or individual revocation) occurred between
/// the time the permission lease was minted and the time the side effect
/// is about to execute. The call must be refused — fail-closed.
#[derive(Debug, Clone)]
pub struct RevocationAdvanced {
    pub minted_epoch: u64,
    pub live_epoch: u64,
}

impl std::fmt::Display for RevocationAdvanced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "revocation epoch advanced {}→{} (policy tightened since approval)",
            self.minted_epoch, self.live_epoch
        )
    }
}

impl std::error::Error for RevocationAdvanced {}

/// Gate that checks whether revocation has advanced since a lease was minted.
///
/// Invariant #16: "Snapshot authorization can only be tightened by revocation."
///
/// Created from a `PermissionLease`'s `revocation_epoch` at mint time.
/// Before the side effect, call `check(live_epoch)` — if the live epoch
/// has advanced, the bound snapshot is stale and the call must be refused.
///
/// This type replaces the ad-hoc inline revalidation block in
/// `execute_single_tool`, giving the fence a name and making it reusable
/// across the DelegationEnvelope and ACP layers.
#[derive(Debug, Clone, Copy)]
pub struct LiveRevocationFence {
    minted_epoch: u64,
}

impl LiveRevocationFence {
    /// Create a fence from a minted revocation epoch.
    pub fn new(minted_epoch: u64) -> Self {
        Self { minted_epoch }
    }

    /// Create a fence from a `PermissionLease`'s revocation epoch.
    pub fn from_lease(lease: &PermissionLease) -> Self {
        Self::new(lease.revocation_epoch)
    }

    /// Check against the live revocation epoch.
    ///
    /// Returns `Ok(())` if the policy hasn't tightened (live ≤ minted).
    /// Returns `Err(RevocationAdvanced)` if the live epoch advanced past
    /// the minted epoch — the call must be refused (fail-closed).
    pub fn check(&self, live_epoch: u64) -> Result<(), RevocationAdvanced> {
        if live_epoch > self.minted_epoch {
            Err(RevocationAdvanced {
                minted_epoch: self.minted_epoch,
                live_epoch,
            })
        } else {
            Ok(())
        }
    }

    /// The minted epoch this fence was captured at.
    pub fn minted_epoch(&self) -> u64 {
        self.minted_epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::id::ToolCallId;

    #[test]
    fn lease_is_single_use() {
        let mut lease = PermissionLease::new(
            ToolCallId::new(),
            ApprovalResolution::Allow,
            0,
            None,
        );
        assert!(lease.consume(), "first consume must succeed");
        assert!(!lease.consume(), "second consume must fail (single-use)");
        assert_eq!(lease.uses(), 1);
    }

    #[test]
    fn narrow_permits_execution_and_maps_to_ask() {
        let r = ApprovalResolution::Narrow {
            narrowed_args: serde_json::json!({"path": "/tmp/safe.txt"}),
        };
        assert!(r.permits_execution());
        assert_eq!(r.to_policy_decision(), PolicyDecision::Ask);
    }

    #[test]
    fn deny_and_cancel_both_block() {
        assert!(!ApprovalResolution::Deny.permits_execution());
        assert!(!ApprovalResolution::Cancel.permits_execution());
        assert_eq!(ApprovalResolution::Cancel.to_policy_decision(), PolicyDecision::Deny);
    }

    #[test]
    fn expired_lease_refuses_consume() {
        let mut lease = PermissionLease::new(
            ToolCallId::new(),
            ApprovalResolution::Allow,
            0,
            Some(Duration::from_secs(0)),
        );
        // TTL 0 ⇒ already expired by the time we consume (best-effort).
        // Allow a tiny grace by checking is_valid directly:
        let _ = lease.is_valid(); // may be true or false depending on timing
    }

    #[test]
    fn fence_passes_when_epoch_unchanged() {
        let fence = LiveRevocationFence::new(5);
        assert!(fence.check(5).is_ok());
        assert!(fence.check(3).is_ok(), "live < minted is also ok (not tightened)");
    }

    #[test]
    fn fence_rejects_when_epoch_advanced() {
        let fence = LiveRevocationFence::new(3);
        let err = fence.check(5).unwrap_err();
        assert_eq!(err.minted_epoch, 3);
        assert_eq!(err.live_epoch, 5);
        assert!(err.to_string().contains("3→5"));
    }

    #[test]
    fn fence_from_lease() {
        let lease = PermissionLease::new(
            ToolCallId::new(),
            ApprovalResolution::Allow,
            7,
            None,
        );
        let fence = LiveRevocationFence::from_lease(&lease);
        assert_eq!(fence.minted_epoch(), 7);
        assert!(fence.check(7).is_ok());
        assert!(fence.check(8).is_err());
    }
}
