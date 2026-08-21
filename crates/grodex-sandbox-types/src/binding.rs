//! Sandbox binding — a concrete sandbox instance attached to an operation.

use crate::profile::SandboxProfile;
use serde::{Deserialize, Serialize};

/// The underlying sandbox implementation technology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxType {
    /// No sandbox (unrestricted — development only).
    None,
    /// macOS Seatbelt sandbox.
    Seatbelt,
    /// Linux Landlock LSM.
    Landlock,
    /// Linux bubblewrap (user namespace + bind mounts).
    Bubblewrap,
    /// Docker container.
    Docker,
}

/// A concrete binding of a sandbox profile to an execution environment.
///
/// Created by the Sandbox Supervisor in response to an approved operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxBinding {
    /// Unique binding identifier.
    pub binding_id: String,
    /// The profile this binding enforces.
    pub profile: SandboxProfile,
    /// Generation of the profile at binding time.
    pub profile_generation: u64,
    /// Which sandbox technology is being used.
    pub sandbox_type: SandboxType,
    /// When this binding was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A policy binding attaches a security policy to a Step/Turn snapshot.
///
/// Design Doc 10 §12.1: the StepSnapshot carries both a SandboxBinding
/// and a PolicyBinding. The PolicyBinding records which policy generation
/// was in effect when the Step was captured, enabling revocation fence
/// checks during replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyBinding {
    /// Policy identifier (e.g. "default", "enterprise-strict").
    pub policy_id: String,
    /// Generation of the policy at binding time.
    pub policy_generation: u64,
    /// Maximum authority ceiling allowed by this policy.
    pub authority_ceiling: u8,
    /// Whether this policy allows deferred capability promotion.
    pub deferred_promotion_allowed: bool,
    /// When this binding was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
}
