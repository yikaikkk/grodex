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

    /// 词法归一（用于尚不存在的路径）：绝对化 + 解析 `.`/`..`。
    fn normalize_lexical(path: &Path) -> std::path::PathBuf {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let mut out = std::path::PathBuf::new();
        for comp in abs.components() {
            use std::path::Component;
            match comp {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out
    }

    /// 路径的所有可比形态：原始字面量、词法归一（解 `.`/`..`）、
    /// 真实形态（`fs::canonicalize`，同时解符号链接，仅在路径存在时可用）。
    /// 这是防 `../` 与符号链接逃逸的关键：校验层与工具层实际打开的必须覆盖
    /// 同一路径的真实形态，否则会出现“校验允许、实际逃逸”的窗口。
    fn forms(path: &Path) -> Vec<String> {
        let mut v = vec![path.to_string_lossy().to_string()];
        let lex = Self::normalize_lexical(path).to_string_lossy().to_string();
        if !v.contains(&lex) {
            v.push(lex);
        }
        if let Ok(c) = std::fs::canonicalize(path) {
            let cs = c.to_string_lossy().to_string();
            if !v.contains(&cs) {
                v.push(cs);
            }
        }
        v
    }

    /// Check if a path matches any pattern in the list.
    /// 路径与模式各自展开全部形态后两两前缀匹配：原始匹配保持向后兼容，
    /// 交叉匹配覆盖 `../`、符号链接、相对路径与 `/tmp → /private/tmp` 之类的别名，
    /// 避免“路径不存在只有词法形态、模式存在有真实形态”的混合错配。
    fn matches_any(patterns: &[String], path: &Path) -> bool {
        let path_forms = Self::forms(path);
        for pattern in patterns {
            for pat_form in Self::forms(Path::new(pattern.trim_end_matches('/'))) {
                let pat_form = pat_form.trim_end_matches('/');
                if path_forms.iter().any(|pf| pf.starts_with(pat_form)) {
                    return true;
                }
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

    #[test]
    fn dotdot_cannot_bypass_deny() {
        // 回归：旧实现是纯字符串前缀匹配，`/tmp/public/../secrets/key` 不以
        // `/tmp/secrets` 开头 → 绕过 deny；又以 `/tmp` 开头 → allow，形成逃逸窗口。
        // 规范化后 deny 必须命中。
        let p = readonly_profile();
        assert!(!PathValidator::can_read(&p, Path::new("/tmp/public/../secrets/key")));
        assert!(!PathValidator::can_write(
            &SandboxProfile {
                name: "rw".into(),
                read_only_paths: vec![],
                read_write_paths: vec!["/tmp".into()],
                deny_paths: vec!["/tmp/secrets".into()],
                network_rules: vec![],
                allow_exec: false,
                allow_fork: false,
            },
            Path::new("/tmp/public/../secrets/key")
        ));
    }

    #[test]
    fn dotdot_resolves_back_into_allow() {
        // `..` 绕出又绕回允许区域，规范化后应仍被允许（不误杀）。
        let p = readonly_profile();
        assert!(PathValidator::can_read(&p, Path::new("/tmp/../tmp/foo.txt")));
        assert!(PathValidator::can_read(&p, Path::new("/tmp/./foo.txt")));
    }
}
