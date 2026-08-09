//! Grodex Auth — authentication and credential management runtime.
//!
//! Provides credential discovery from environment variables, API key
//! resolution, and provider account management. Secrets are loaded
//! at call time and never persisted in memory.
//!
//! The `CredentialBroker` (in `lease`) is the trusted holder of master
//! tokens; agents only ever see bounded `CredentialLease`s and redeem them
//! via `broker.resolve`. No master token crosses a struct boundary that
//! promises not to log/persist it.
//!
//! The `McpoAuthBroker` (in `mcp_oauth`) is the skeleton state-machine for
//! running OAuth 2.0 authorization-code flows (with PKCE + nonce + state)
//! against MCP servers. Provider-specific HTTP calls are delegated; the
//! broker owns registration, URL synthesis, state validation, and lease
//! handoff to CredentialBroker.

pub mod lease;
pub mod manager;
pub mod mcp_oauth;
pub mod secret_store;
pub mod store;

#[cfg(target_os = "macos")]
pub mod keychain_store;

pub use lease::{CredentialBroker, CredentialError, LeaseError};
pub use manager::AuthManager;
pub use mcp_oauth::{
    AuthorizationUrl, CredentialLeaseId, McpoAuthBroker, McpoAuthError, OAuthClientConfig,
    PendingAuthorization, ServerId,
};
pub use secret_store::{InMemorySecretStore, SecretStore, SecretStoreError};
pub use store::CredentialStore;

#[cfg(target_os = "macos")]
pub use keychain_store::MacKeychainSecretStore;
