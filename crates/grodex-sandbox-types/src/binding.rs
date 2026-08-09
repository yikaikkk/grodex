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
