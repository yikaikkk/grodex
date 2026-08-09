//! SandboxManager — session-scoped sandbox coordinator.
//!
//! Holds profiles and provides high-level validation methods for the
//! Tool Pipeline and Agent Loop.

use crate::profile::ProfileStore;
use crate::validator::PathValidator;
use grodex_sandbox_types::profile::SandboxProfile;
use std::path::Path;

/// Session-scoped sandbox manager.
///
/// Created once per session. Validates file, exec, and network operations
/// against the active sandbox profile.
#[derive(Debug)]
pub struct SandboxManager {
    store: ProfileStore,
    active_profile: String,
}

impl SandboxManager {
    /// Create a new manager with the given active profile.
    pub fn new(active_profile: impl Into<String>) -> Self {
        Self {
            store: ProfileStore::new(),
            active_profile: active_profile.into(),
        }
    }

    /// Get the active profile.
    pub fn active_profile(&self) -> Option<&SandboxProfile> {
        self.store.get(&self.active_profile)
    }

    /// Switch the active profile.
    pub fn set_profile(&mut self, name: impl Into<String>) {
        self.active_profile = name.into();
    }

    /// Register a custom profile.
    pub fn register_profile(&mut self, profile: SandboxProfile) {
        self.store.register(profile);
    }

    /// Check if reading a path is permitted.
    pub fn validate_read(&self, path: &Path) -> Result<(), String> {
        match self.active_profile() {
            Some(profile) if PathValidator::can_read(profile, path) => Ok(()),
            Some(_) => Err(format!("read denied for: {}", path.display())),
            None => Err("no active sandbox profile".into()),
        }
    }

    /// Check if writing a path is permitted.
    pub fn validate_write(&self, path: &Path) -> Result<(), String> {
        match self.active_profile() {
            Some(profile) if PathValidator::can_write(profile, path) => Ok(()),
            Some(_) => Err(format!("write denied for: {}", path.display())),
            None => Err("no active sandbox profile".into()),
        }
    }

    /// Check if executing commands is permitted.
    pub fn validate_exec(&self) -> Result<(), String> {
        match self.active_profile() {
            Some(profile) if PathValidator::can_exec(profile) => Ok(()),
            Some(_) => Err("exec denied".into()),
            None => Err("no active sandbox profile".into()),
        }
    }

    /// Check if connecting to a host is permitted.
    pub fn validate_network(&self, host: &str) -> Result<(), String> {
        match self.active_profile() {
            Some(profile) if PathValidator::can_connect(profile, host) => Ok(()),
            Some(_) => Err(format!("network denied for: {host}")),
            None => Err("no active sandbox profile".into()),
        }
    }
}

impl Default for SandboxManager {
    fn default() -> Self {
        Self::new("workspace")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_workspace() {
        let mgr = SandboxManager::default();
        assert!(mgr.active_profile().is_some());
        assert!(mgr.validate_read(Path::new("/tmp/test.txt")).is_ok());
        assert!(mgr.validate_write(Path::new("./output.txt")).is_ok());
    }

    #[test]
    fn readonly_denies_write() {
        let mut mgr = SandboxManager::default();
        mgr.set_profile("readonly");
        assert!(mgr.validate_write(Path::new("./out.txt")).is_err());
    }

    #[test]
    fn restricted_denies_everything() {
        let mut mgr = SandboxManager::default();
        mgr.set_profile("restricted");
        assert!(mgr.validate_read(Path::new("/tmp")).is_err());
        assert!(mgr.validate_exec().is_err());
        assert!(mgr.validate_network("example.com").is_err());
    }
}
