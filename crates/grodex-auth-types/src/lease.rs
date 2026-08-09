//! Credential Lease — a short-lived, single-purpose token grant.
//!
//! Leases are issued by the Credential Broker for one request (or a small
//! batch). They carry strict bounds: endpoint, max uses, and TTL.

use serde::{Deserialize, Serialize};

/// A time-and-scope-bounded credential lease for one operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialLease {
    /// Unique lease identifier for auditing.
    pub lease_id: String,
    /// The handle this lease was derived from.
    pub handle_id: String,
    /// Which specific endpoint this lease is valid for.
    pub endpoint_binding: String,
    /// Process or container identity that is authorized to use this lease.
    pub issued_to_process_identity: String,
    /// Maximum number of times this lease may be used.
    pub max_uses: u32,
    /// Absolute expiration time.
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// Revocation epoch — leases from earlier epochs are rejected.
    pub revocation_epoch: u64,
}
