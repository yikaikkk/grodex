//! PathValidator — checks whether a filesystem path is allowed by a sandbox profile.

use grodex_sandbox_types::profile::{NetworkRule, SandboxProfile};
use std::path::Path;

/// Validates filesystem and network operations against a sandbox profile.
pub struct PathValidator;

impl PathValidator {
    /// Check if reading `path` is allowed.
    pub fn can_read(profile: &SandboxProfile, path: &Path) -> bool {
        // Deny takes precedence.
        if Self::matches_any(&profile.deny_paths, path) {
            return false;
        }
        // Must match at least one read-only or read-write path.
        Self::matches_any(&profile.read_only_paths, path) || Self::matches_any(&profile.read_write_paths, path)
    }

    /// Check if writing `path` is allowed.
    pub fn can_write(profile: &SandboxProfile, path: &Path) -> bool {
        if Self::matches_any(&profile.deny_paths, path) {
            return false;
        }
        Self::matches_any(&profile.read_write_paths, path)
    }

    /// Check if network access to `host` is allowed.
    pub fn can_connect(profile: &SandboxProfile, host: &str) -> bool {
        for rule in &profile.network_rules {
            match rule {
                NetworkRule::DenyAll => return false,
                NetworkRule::Allow(pattern) => {
                    if pattern == host || pattern == "*" {
                        return true;
                    }
                }
                NetworkRule::Deny(pattern) => {
                    if pattern == host {
                        return false;
                    }
                }
                NetworkRule::AllowLocal => {
                    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
                        return true;
                    }
                }
            }
        }
        // No matching allow rule → deny by default.
        profile.network_rules.is_empty()
    }

    /// Check if executing commands is allowed.
    pub fn can_exec(profile: &SandboxProfile) -> bool {
        profile.allow_exec
    }

    /// Normalize a path for matching.
    fn normalize(path: &Path) -> String {
        path.to_string_lossy().to_string()
    }

    /// Check if a path matches any pattern in the list.
    fn matches_any(patterns: &[String], path: &Path) -> bool {
        let path_str = Self::normalize(path);
        for pattern in patterns {
            if path_str.starts_with(pattern.trim_end_matches('/')) || path_str == *pattern {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_sandbox_types::profile::SandboxProfile;

    fn readonly_profile() -> SandboxProfile {
        SandboxProfile {
            name: "test".into(),
            read_only_paths: vec!["/tmp".into(), "/home".into()],
            read_write_paths: vec![],
            deny_paths: vec!["/tmp/secrets".into()],
            network_rules: vec![],
            allow_exec: false,
            allow_fork: false,
        }
    }

    #[test]
    fn read_allowed_in_ro_path() {
        let p = readonly_profile();
        assert!(PathValidator::can_read(&p, Path::new("/tmp/foo.txt")));
    }

    #[test]
    fn read_denied_for_deny_path() {
        let p = readonly_profile();
        assert!(!PathValidator::can_read(&p, Path::new("/tmp/secrets/key")));
    }

    #[test]
    fn write_denied_for_readonly_profile() {
        let p = readonly_profile();
        assert!(!PathValidator::can_write(&p, Path::new("/tmp/foo.txt")));
    }

    #[test]
    fn network_deny_all_blocks_everything() {
        let mut p = readonly_profile();
        p.network_rules = vec![NetworkRule::DenyAll];
        assert!(!PathValidator::can_connect(&p, "example.com"));
    }

    #[test]
    fn exec_blocked_when_disabled() {
        let p = readonly_profile();
        assert!(!PathValidator::can_exec(&p));
    }
}
