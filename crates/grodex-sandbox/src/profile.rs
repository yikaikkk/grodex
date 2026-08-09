//! ProfileStore — built-in and user-defined sandbox profiles.

use crate::profile_layers::{
    intersect_layers, AccessLevel, ExtraProfileHints, IntersectionResult, LayeredProfileInput,
    ProfileLayer,
};
use crate::runtime::PreparedOperation;
use grodex_sandbox_types::profile::{NetworkRule, SandboxProfile};
use std::collections::HashMap;

/// A store of named sandbox profiles.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    profiles: HashMap<String, SandboxProfile>,
}

impl ProfileStore {
    /// Create a store with built-in profiles.
    pub fn new() -> Self {
        let mut store = Self {
            profiles: HashMap::new(),
        };
        store.register_builtins();
        store
    }

    /// Register the built-in profiles.
    fn register_builtins(&mut self) {
        self.profiles.insert(
            "readonly".into(),
            SandboxProfile {
                name: "readonly".into(),
                read_only_paths: vec!["/".into()],
                read_write_paths: vec![],
                deny_paths: vec!["/etc/ssh".into(), "/etc/ssl/private".into()],
                network_rules: vec![],
                allow_exec: false,
                allow_fork: false,
            },
        );

        self.profiles.insert(
            "workspace".into(),
            SandboxProfile {
                name: "workspace".into(),
                read_only_paths: vec!["/".into()],
                read_write_paths: vec![".".into()],
                deny_paths: vec!["/etc".into(), "/System".into(), "~/.ssh".into()],
                network_rules: vec![NetworkRule::AllowLocal],
                allow_exec: true,
                allow_fork: true,
            },
        );

        self.profiles.insert(
            "restricted".into(),
            SandboxProfile {
                name: "restricted".into(),
                read_only_paths: vec![],
                read_write_paths: vec![],
                deny_paths: vec!["/".into()],
                network_rules: vec![NetworkRule::DenyAll],
                allow_exec: false,
                allow_fork: false,
            },
        );

        self.profiles.insert(
            "full".into(),
            SandboxProfile {
                name: "full".into(),
                read_only_paths: vec!["/".into()],
                read_write_paths: vec!["/".into()],
                deny_paths: vec![],
                network_rules: vec![],
                allow_exec: true,
                allow_fork: true,
            },
        );
    }

    /// Register a custom profile.
    pub fn register(&mut self, profile: SandboxProfile) {
        self.profiles.insert(profile.name.clone(), profile);
    }

    /// Get a profile by name.
    pub fn get(&self, name: &str) -> Option<&SandboxProfile> {
        self.profiles.get(name)
    }

    /// List all profile names.
    pub fn list(&self) -> Vec<&str> {
        self.profiles.keys().map(|s| s.as_str()).collect()
    }

    /// 按 AccessLevel 过滤 LayeredProfileInput，并执行 7 层求交。
    pub fn resolve_layered(
        &self,
        input: &LayeredProfileInput,
        level: AccessLevel,
    ) -> IntersectionResult {
        let mut filtered = input.clone();
        let mut diagnostics: Vec<String> = Vec::new();

        for layer in [
            ProfileLayer::PolicyCeiling,
            ProfileLayer::UserBinding,
            ProfileLayer::Capability,
            ProfileLayer::Tool,
            ProfileLayer::Supervisor,
            ProfileLayer::OS,
            ProfileLayer::Default,
        ] {
            if !level.allows_layer(layer) {
                let slot = filtered.get_layer_mut(layer);
                if slot.is_some() {
                    let removed = slot.take();
                    if let Some(profile) = removed {
                        diagnostics.push(format!(
                            "{} binding (profile={}) 被 AccessLevel={:?} 忽略",
                            layer.as_str(),
                            profile.name,
                            level
                        ));
                    }
                }
            }
        }

        let mut result = intersect_layers(&filtered);
        diagnostics.extend(result.diagnostics);
        result.diagnostics = diagnostics;
        result
    }

    /// 根据 PreparedOperation + ExtraProfileHints 从 store 查命名 profile 生成 LayeredProfileInput。
    pub fn build_input_for_operation(
        &self,
        _op: &PreparedOperation,
        extra: &ExtraProfileHints<'_>,
    ) -> LayeredProfileInput {
        LayeredProfileInput {
            policy_ceiling: extra
                .policy_ceiling_name
                .and_then(|n| self.get(n))
                .cloned(),
            user_binding: extra
                .user_binding_name
                .and_then(|n| self.get(n))
                .cloned(),
            capability: extra
                .capability_profile_name
                .and_then(|n| self.get(n))
                .cloned(),
            tool: extra
                .tool_profile_name
                .and_then(|n| self.get(n))
                .cloned(),
            supervisor: extra
                .supervisor_profile_name
                .and_then(|n| self.get(n))
                .cloned(),
            os_default: Some(self.get(extra.os_default_name).cloned().unwrap_or_else(|| {
                SandboxProfile {
                    name: format!("os-default-fallback({})", extra.os_default_name),
                    read_only_paths: vec![],
                    read_write_paths: vec![],
                    deny_paths: vec!["/".into()],
                    network_rules: vec![NetworkRule::DenyAll],
                    allow_exec: false,
                    allow_fork: false,
                }
            })),
            default: Some(self.get(extra.default_name).cloned().unwrap_or_else(|| {
                SandboxProfile {
                    name: format!("default-fallback({})", extra.default_name),
                    read_only_paths: vec!["/".into()],
                    read_write_paths: vec![".".into()],
                    deny_paths: vec![],
                    network_rules: vec![NetworkRule::AllowLocal],
                    allow_exec: true,
                    allow_fork: true,
                }
            })),
        }
    }
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_layers::{
        intersect_layers, intersect_profiles, AccessLevel, LayeredProfileInput, ProfileLayer,
    };
    use grodex_sandbox_types::profile::{NetworkRule, SandboxProfile};

    fn tool_allow_local() -> SandboxProfile {
        SandboxProfile {
            name: "tool-allow-local".into(),
            read_only_paths: vec!["/".into()],
            read_write_paths: vec![".".into()],
            deny_paths: vec![],
            network_rules: vec![NetworkRule::AllowLocal],
            allow_exec: true,
            allow_fork: true,
        }
    }

    fn policy_deny_all() -> SandboxProfile {
        SandboxProfile {
            name: "policy-deny-all".into(),
            read_only_paths: vec!["/".into()],
            read_write_paths: vec![".".into()],
            deny_paths: vec![],
            network_rules: vec![NetworkRule::DenyAll],
            allow_exec: true,
            allow_fork: true,
        }
    }

    #[test]
    fn test_intersect_deny_all_network_absorbs_all() {
        let input = LayeredProfileInput {
            policy_ceiling: Some(policy_deny_all()),
            tool: Some(tool_allow_local()),
            ..Default::default()
        };
        let result = intersect_layers(&input);
        assert!(
            result
                .effective
                .network_rules
                .iter()
                .any(|r| matches!(r, NetworkRule::DenyAll)),
            "DenyAll 应该吸收所有 Allow*；实际 network_rules: {:?}",
            result.effective.network_rules
        );
        assert!(result.contributing_layers.contains(&ProfileLayer::PolicyCeiling));
        assert!(result.contributing_layers.contains(&ProfileLayer::Tool));
    }

    #[test]
    fn test_intersect_write_paths_strictest_only() {
        let user_layer = SandboxProfile {
            name: "user-layer".into(),
            read_only_paths: vec!["/".into()],
            read_write_paths: vec!["./a".into(), "./b".into()],
            deny_paths: vec![],
            network_rules: vec![NetworkRule::AllowLocal],
            allow_exec: true,
            allow_fork: true,
        };
        let default_layer = SandboxProfile {
            name: "default-layer".into(),
            read_only_paths: vec!["/".into()],
            read_write_paths: vec!["/".into()],
            deny_paths: vec![],
            network_rules: vec![NetworkRule::AllowLocal],
            allow_exec: true,
            allow_fork: true,
        };
        let result = intersect_profiles(&user_layer, &default_layer);
        assert!(
            result.read_write_paths.contains(&"./a".to_string()),
            "交集应包含 ./a；实际: {:?}",
            result.read_write_paths
        );
        assert!(
            result.read_write_paths.contains(&"./b".to_string()),
            "交集应包含 ./b；实际: {:?}",
            result.read_write_paths
        );
        assert!(
            !result.read_write_paths.contains(&"/".to_string()),
            "交集不应包含 /；实际: {:?}",
            result.read_write_paths
        );
    }

    #[test]
    fn test_level0_blocks_user_tool_binding() {
        let store = ProfileStore::new();
        let full = store.get("full").unwrap().clone();
        let input = LayeredProfileInput {
            policy_ceiling: Some(store.get("readonly").unwrap().clone()),
            user_binding: Some(full.clone()),
            tool: Some(full.clone()),
            os_default: Some(store.get("restricted").unwrap().clone()),
            default: Some(store.get("workspace").unwrap().clone()),
            ..Default::default()
        };
        let result = store.resolve_layered(&input, AccessLevel::Level0);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.contains("UserBinding") && d.contains("忽略")),
            "Level0 应忽略 UserBinding；diagnostics: {:?}",
            result.diagnostics
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.contains("Tool") && d.contains("忽略")),
            "Level0 应忽略 Tool；diagnostics: {:?}",
            result.diagnostics
        );
        assert!(
            !result.contributing_layers.contains(&ProfileLayer::UserBinding),
            "contributing_layers 不应含 UserBinding"
        );
        assert!(
            !result.contributing_layers.contains(&ProfileLayer::Tool),
            "contributing_layers 不应含 Tool"
        );
        assert!(result.contributing_layers.contains(&ProfileLayer::PolicyCeiling));
        assert!(result.contributing_layers.contains(&ProfileLayer::OS));
        assert!(result.contributing_layers.contains(&ProfileLayer::Default));
    }

    #[test]
    fn test_level1_blocks_user_tool_supervisor() {
        let store = ProfileStore::new();
        let full = store.get("full").unwrap().clone();
        let input = LayeredProfileInput {
            policy_ceiling: Some(store.get("readonly").unwrap().clone()),
            user_binding: Some(full.clone()),
            tool: Some(full.clone()),
            supervisor: Some(full.clone()),
            capability: Some(store.get("workspace").unwrap().clone()),
            os_default: Some(store.get("restricted").unwrap().clone()),
            default: Some(store.get("workspace").unwrap().clone()),
        };
        let result = store.resolve_layered(&input, AccessLevel::Level1);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.contains("Supervisor") && d.contains("忽略")),
            "Level1 应忽略 Supervisor；diagnostics: {:?}",
            result.diagnostics
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.contains("UserBinding") && d.contains("忽略")),
            "Level1 应忽略 UserBinding"
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.contains("Tool") && d.contains("忽略")),
            "Level1 应忽略 Tool"
        );
        assert!(
            !result.contributing_layers.contains(&ProfileLayer::UserBinding),
            "contributing_layers 不应含 UserBinding"
        );
        assert!(
            !result.contributing_layers.contains(&ProfileLayer::Tool),
            "contributing_layers 不应含 Tool"
        );
        assert!(
            !result.contributing_layers.contains(&ProfileLayer::Supervisor),
            "contributing_layers 不应含 Supervisor"
        );
        assert!(result.contributing_layers.contains(&ProfileLayer::Capability));
    }

    #[test]
    fn test_empty_readwrite_fail_closed() {
        let a = SandboxProfile {
            name: "only-a".into(),
            read_only_paths: vec!["/".into()],
            read_write_paths: vec!["./a".into()],
            deny_paths: vec![],
            network_rules: vec![NetworkRule::AllowLocal],
            allow_exec: true,
            allow_fork: true,
        };
        let b = SandboxProfile {
            name: "only-b".into(),
            read_only_paths: vec!["/".into()],
            read_write_paths: vec!["./b".into()],
            deny_paths: vec![],
            network_rules: vec![NetworkRule::AllowLocal],
            allow_exec: true,
            allow_fork: true,
        };
        let result = intersect_profiles(&a, &b);
        assert!(
            result.read_write_paths.is_empty(),
            "互斥 read_write_paths 交集应为空；实际: {:?}",
            result.read_write_paths
        );
        assert!(
            result.deny_paths.iter().any(|p| p == "/"),
            "交集为空时 fail-closed 应追加 deny_paths=['/']；实际 deny_paths: {:?}",
            result.deny_paths
        );

        let layered = LayeredProfileInput {
            user_binding: Some(a),
            default: Some(b),
            ..Default::default()
        };
        let multi = intersect_layers(&layered);
        assert!(
            multi
                .diagnostics
                .iter()
                .any(|d| d.contains("read_write_paths 交集为空")),
            "应生成 diagnostic；实际 diagnostics: {:?}",
            multi.diagnostics
        );
    }
}
