//! Credential Handle — an opaque reference to a credential.
//!
//! Agents never see the actual token. They hold a handle that the Credential
//! Broker exchanges for short-lived leases at request time.

use grodex_core::id::SessionId;
use serde::{Deserialize, Serialize};

/// An opaque handle referencing a credential managed by the Credential Broker.
///
/// The Agent passes this handle when making model or MCP requests. The
/// Gateway resolves it to a concrete token only at request time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialHandle {
    /// Unique handle identifier.
    pub handle_id: String,
    /// The account this handle references.
    pub account_id: String,
    /// The provider this credential is for.
    pub provider_id: String,
    /// Intended audience for this credential.
    pub audience: String,
    /// Maximum allowed scopes (the actual token may have fewer).
    pub scope_ceiling: Vec<String>,
    /// The session that owns this handle.
    pub session_id: SessionId,
    /// When this handle expires (not the underlying token).
    pub expires_at: chrono::DateTime<chrono::Utc>,
}
