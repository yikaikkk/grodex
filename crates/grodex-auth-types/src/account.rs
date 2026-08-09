//! Account descriptor — identifies one account/provider binding.

use serde::{Deserialize, Serialize};

/// How the user authenticates with this account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    /// A static API key string.
    ApiKey,
    /// OAuth 2.0 authorization code flow.
    OAuth2,
    /// Device code flow for headless environments.
    DeviceCode,
    /// Externally managed credential (e.g. from a CI environment variable).
    External,
}

/// Current status of an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountStatus {
    /// The credential is valid and usable.
    Active,
    /// The token has expired and needs refresh.
    Expired,
    /// The account has been explicitly revoked.
    Revoked,
    /// An unexpected error occurred during the last auth attempt.
    Error,
}

/// Describes one account/provider binding.
///
/// An account is the long-lived identity. It is NOT the credential itself —
/// credentials are managed by the Credential Broker and accessed through
/// opaque handles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountDescriptor {
    /// Unique account identifier.
    pub account_id: String,
    /// The provider this account is for (e.g. "anthropic", "openai").
    pub provider_id: String,
    /// Human-readable principal (email, service account name).
    pub principal_display: String,
    /// Which authentication method this account uses.
    pub auth_method: AuthMethod,
    /// Optional tenant or workspace within the provider.
    pub tenant_id: Option<String>,
    /// OAuth scopes this account is authorized for.
    pub scopes: Vec<String>,
    /// Intended audience(s) for the credential.
    pub audiences: Vec<String>,
    /// Current status.
    pub status: AccountStatus,
    /// Monotonic generation counter for change detection.
    pub metadata_generation: u64,
}
