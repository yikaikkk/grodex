//! Grodex Auth Types — authentication and credential data model.
//!
//! Agents never hold long-term secrets. They reference accounts through
//! opaque `CredentialHandle` values, and the Credential Broker issues
//! short-lived `CredentialLease` values for individual requests.

pub mod account;
pub mod handle;
pub mod lease;
