//! Sandbox profile — defines the filesystem and network boundaries for a sandbox.

use serde::{Deserialize, Serialize};

/// A filesystem or network access rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkRule {
    /// Allow access to a specific host:port.
    Allow(String),
    /// Block access to a specific host:port.
    Deny(String),
    /// Allow all localhost connections.
    AllowLocal,
    /// Block all network access.
    DenyAll,
}

/// A named sandbox profile defining filesystem and network access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxProfile {
    /// Unique profile name.
    pub name: String,
    /// Paths the sandbox may read.
    pub read_only_paths: Vec<String>,
    /// Paths the sandbox may read and write.
    pub read_write_paths: Vec<String>,
    /// Paths explicitly denied (even if a parent is allowed).
    pub deny_paths: Vec<String>,
    /// Network access rules.
    pub network_rules: Vec<NetworkRule>,
    /// Whether the sandbox allows executing new processes.
    pub allow_exec: bool,
    /// Whether the sandbox allows forking.
    pub allow_fork: bool,
}
